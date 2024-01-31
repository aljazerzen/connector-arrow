//! Source implementation for Postgres database, including the TLS support (client only).

mod connection;
mod errors;
mod typesystem;

pub use self::errors::PostgresSourceError;
pub use connection::rewrite_tls_args;
use itertools::zip_eq;
pub use typesystem::{PostgresTypePairs, PostgresTypeSystem};

use crate::constants::DB_BUFFER_SIZE;
use crate::typesystem::Schema;
use crate::{
    data_order::DataOrder,
    errors::ConnectorXError,
    sources::{Produce, Source, SourceReader, ValueStream},
    sql::CXQuery,
};
use anyhow::anyhow;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use csv::{ReaderBuilder, StringRecord, StringRecordsIntoIter};
use fehler::{throw, throws};
use hex::decode;
use postgres::{
    binary_copy::{BinaryCopyOutIter, BinaryCopyOutRow},
    fallible_iterator::FallibleIterator,
    tls::{MakeTlsConnect, TlsConnect},
    Config, CopyOutReader, Row, RowIter, SimpleQueryMessage, Socket,
};
use r2d2::{Pool, PooledConnection};
use r2d2_postgres::PostgresConnectionManager;
use rust_decimal::Decimal;
use serde_json::{from_str, Value};

use std::collections::HashMap;

use std::marker::PhantomData;
use uuid::Uuid;

/// Protocol - Binary based bulk load
pub enum BinaryProtocol {}

/// Protocol - CSV based bulk load
pub enum CSVProtocol {}

/// Protocol - use Cursor
pub enum CursorProtocol {}

/// Protocol - use Simple Query
pub enum SimpleProtocol {}

type PgManager<C> = PostgresConnectionManager<C>;
type PgConn<C> = PooledConnection<PgManager<C>>;

pub struct PostgresSource<P, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    pool: Pool<PgManager<C>>,
    _protocol: PhantomData<P>,
}

impl<P, C> PostgresSource<P, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    #[throws(PostgresSourceError)]
    pub fn new(config: Config, tls: C, nconn: usize) -> Self {
        let manager = PostgresConnectionManager::new(config, tls);
        let pool = Pool::builder().max_size(nconn as u32).build(manager)?;

        Self {
            pool,
            _protocol: PhantomData,
        }
    }
}

impl<P, C> Source for PostgresSource<P, C>
where
    PostgresReader<P, C>:
        SourceReader<TypeSystem = PostgresTypeSystem, Error = PostgresSourceError>,
    P: Send,
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    const DATA_ORDERS: &'static [DataOrder] = &[DataOrder::RowMajor];
    type Reader = PostgresReader<P, C>;
    type TypeSystem = PostgresTypeSystem;
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn reader(&mut self, query: &CXQuery, data_order: DataOrder) -> Self::Reader {
        if !matches!(data_order, DataOrder::RowMajor) {
            throw!(ConnectorXError::UnsupportedDataOrder(data_order));
        }

        let conn = self.pool.get()?;

        PostgresReader::<P, C>::new(conn, query)
    }
}

pub struct PostgresReader<P, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    conn: PgConn<C>,
    query: CXQuery<String>,

    _protocol: PhantomData<P>,
}

impl<P, C> PostgresReader<P, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    pub fn new(conn: PgConn<C>, query: &CXQuery<String>) -> Self {
        Self {
            conn,
            query: query.clone(),
            _protocol: PhantomData,
        }
    }

    #[throws(PostgresSourceError)]
    fn fetch_metadata_generic(&mut self) -> Schema<PostgresTypeSystem> {
        let stmt = self.conn.prepare(self.query.as_str())?;

        let (names, pg_types): (Vec<String>, Vec<postgres::types::Type>) = stmt
            .columns()
            .iter()
            .map(|col| (col.name().to_string(), col.type_().clone()))
            .unzip();

        let types = pg_types.iter().map(PostgresTypeSystem::from).collect();
        Schema { names, types }
    }
}

impl<C> SourceReader for PostgresReader<BinaryProtocol, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    type TypeSystem = PostgresTypeSystem;
    type Stream<'a> = PostgresBinarySourcePartitionParser<'a>;
    type Error = PostgresSourceError;

    fn fetch_until_schema(&mut self) -> Result<Schema<Self::TypeSystem>, Self::Error> {
        self.fetch_metadata_generic()
    }

    #[throws(PostgresSourceError)]
    fn value_stream(&mut self, schema: &Schema<PostgresTypeSystem>) -> Self::Stream<'_> {
        // this could have been done in fetch metadata
        // but that function might not have been called on this reader
        // so we call it again here
        let stmt = self.conn.prepare(self.query.as_str())?;
        let pg_types: Vec<postgres::types::Type> = stmt
            .columns()
            .iter()
            .map(|col| col.type_().clone())
            .collect();
        let types: Vec<_> = pg_types.iter().map(PostgresTypeSystem::from).collect();
        let pg_schema: Vec<_> = zip_eq(&types, &pg_types)
            .map(|(t1, t2)| postgres::types::Type::from(PostgresTypePairs(t2, t1)))
            .collect();

        let query = format!("COPY ({}) TO STDOUT WITH BINARY", self.query);
        let reader = self.conn.copy_out(&*query)?; // unless reading the data, it seems like issue the query is fast
        let iter = BinaryCopyOutIter::new(reader, &pg_schema);

        PostgresBinarySourcePartitionParser::new(iter, schema)
    }
}

impl<C> SourceReader for PostgresReader<CSVProtocol, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    type TypeSystem = PostgresTypeSystem;
    type Stream<'a> = PostgresCSVStream<'a>;
    type Error = PostgresSourceError;

    fn fetch_until_schema(&mut self) -> Result<Schema<Self::TypeSystem>, Self::Error> {
        self.fetch_metadata_generic()
    }

    #[throws(PostgresSourceError)]
    fn value_stream(&mut self, schema: &Schema<PostgresTypeSystem>) -> Self::Stream<'_> {
        let query = format!("COPY ({}) TO STDOUT WITH CSV", self.query);
        let reader = self.conn.copy_out(&*query)?; // unless reading the data, it seems like issue the query is fast
        let iter = ReaderBuilder::new()
            .has_headers(false)
            .from_reader(reader)
            .into_records();

        PostgresCSVStream::new(iter, schema)
    }
}

impl<C> SourceReader for PostgresReader<CursorProtocol, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    type TypeSystem = PostgresTypeSystem;
    type Stream<'a> = PostgresRawStream<'a>;
    type Error = PostgresSourceError;

    fn fetch_until_schema(&mut self) -> Result<Schema<Self::TypeSystem>, Self::Error> {
        self.fetch_metadata_generic()
    }

    #[throws(PostgresSourceError)]
    fn value_stream(&mut self, schema: &Schema<PostgresTypeSystem>) -> Self::Stream<'_> {
        let q = self.query.as_str();
        let iter = self.conn.query_raw::<_, bool, _>(q, vec![])?; // unless reading the data, it seems like issue the query is fast
        PostgresRawStream::new(iter, schema)
    }
}
pub struct PostgresBinarySourcePartitionParser<'a> {
    iter: BinaryCopyOutIter<'a>,
    rowbuf: Vec<BinaryCopyOutRow>,
    ncols: usize,
    current_col: usize,
    current_row: usize,
    is_finished: bool,
}

impl<'a> PostgresBinarySourcePartitionParser<'a> {
    pub fn new(iter: BinaryCopyOutIter<'a>, schema: &Schema<PostgresTypeSystem>) -> Self {
        Self {
            iter,
            rowbuf: Vec::with_capacity(DB_BUFFER_SIZE),
            ncols: schema.len(),
            current_row: 0,
            current_col: 0,
            is_finished: false,
        }
    }

    #[throws(PostgresSourceError)]
    fn next_loc(&mut self) -> (usize, usize) {
        let ret = (self.current_row, self.current_col);
        self.current_row += (self.current_col + 1) / self.ncols;
        self.current_col = (self.current_col + 1) % self.ncols;
        ret
    }
}

impl<'a> ValueStream<'a> for PostgresBinarySourcePartitionParser<'a> {
    type TypeSystem = PostgresTypeSystem;
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn fetch_batch(&mut self) -> (usize, bool) {
        assert!(self.current_col == 0);
        if self.is_finished {
            return (0, true);
        }

        let remaining_rows = self.rowbuf.len() - self.current_row;
        if remaining_rows > 0 {
            return (remaining_rows, self.is_finished);
        } else if self.is_finished {
            return (0, self.is_finished);
        }

        // clear the buffer
        if !self.rowbuf.is_empty() {
            self.rowbuf.drain(..);
        }
        for _ in 0..DB_BUFFER_SIZE {
            match self.iter.next()? {
                Some(row) => {
                    self.rowbuf.push(row);
                }
                None => {
                    self.is_finished = true;
                    break;
                }
            }
        }

        // reset current cursor positions
        self.current_row = 0;
        self.current_col = 0;

        (self.rowbuf.len(), self.is_finished)
    }
}

macro_rules! impl_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for PostgresBinarySourcePartitionParser<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let row = &self.rowbuf[ridx];
                    let val = row.try_get(cidx)?;
                    val
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for PostgresBinarySourcePartitionParser<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let row = &self.rowbuf[ridx];
                    let val = row.try_get(cidx)?;
                    val
                }
            }
        )+
    };
}

impl_produce!(
    i8,
    i16,
    i32,
    i64,
    f32,
    f64,
    Decimal,
    Vec<i16>,
    Vec<i32>,
    Vec<i64>,
    Vec<f32>,
    Vec<f64>,
    Vec<Decimal>,
    bool,
    Vec<bool>,
    &'r str,
    Vec<u8>,
    NaiveTime,
    NaiveDateTime,
    DateTime<Utc>,
    NaiveDate,
    Uuid,
    Value,
    Vec<String>,
);

impl<'r, 'a> Produce<'r, HashMap<String, Option<String>>>
    for PostgresBinarySourcePartitionParser<'a>
{
    type Error = PostgresSourceError;
    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> HashMap<String, Option<String>> {
        unimplemented!("Please use `cursor` protocol for hstore type");
    }
}

impl<'r, 'a> Produce<'r, Option<HashMap<String, Option<String>>>>
    for PostgresBinarySourcePartitionParser<'a>
{
    type Error = PostgresSourceError;
    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<HashMap<String, Option<String>>> {
        unimplemented!("Please use `cursor` protocol for hstore type");
    }
}

pub struct PostgresCSVStream<'a> {
    iter: StringRecordsIntoIter<CopyOutReader<'a>>,
    rowbuf: Vec<StringRecord>,
    ncols: usize,
    current_col: usize,
    current_row: usize,
    is_finished: bool,
}

impl<'a> PostgresCSVStream<'a> {
    pub fn new(
        iter: StringRecordsIntoIter<CopyOutReader<'a>>,
        schema: &Schema<PostgresTypeSystem>,
    ) -> Self {
        Self {
            iter,
            rowbuf: Vec::with_capacity(DB_BUFFER_SIZE),
            ncols: schema.len(),
            current_row: 0,
            current_col: 0,
            is_finished: false,
        }
    }

    #[throws(PostgresSourceError)]
    fn next_loc(&mut self) -> (usize, usize) {
        let ret = (self.current_row, self.current_col);
        self.current_row += (self.current_col + 1) / self.ncols;
        self.current_col = (self.current_col + 1) % self.ncols;
        ret
    }
}

impl<'a> ValueStream<'a> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;
    type TypeSystem = PostgresTypeSystem;

    #[throws(PostgresSourceError)]
    fn fetch_batch(&mut self) -> (usize, bool) {
        assert!(self.current_col == 0);
        if self.is_finished {
            return (0, true);
        }

        let remaining_rows = self.rowbuf.len() - self.current_row;
        if remaining_rows > 0 {
            return (remaining_rows, self.is_finished);
        } else if self.is_finished {
            return (0, self.is_finished);
        }

        if !self.rowbuf.is_empty() {
            self.rowbuf.drain(..);
        }
        for _ in 0..DB_BUFFER_SIZE {
            if let Some(row) = self.iter.next() {
                self.rowbuf.push(row?);
            } else {
                self.is_finished = true;
                break;
            }
        }
        self.current_row = 0;
        self.current_col = 0;
        (self.rowbuf.len(), self.is_finished)
    }
}

macro_rules! impl_csv_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for PostgresCSVStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    self.rowbuf[ridx][cidx].parse().map_err(|_| {
                        ConnectorXError::cannot_produce::<$t>(Some(self.rowbuf[ridx][cidx].into()))
                    })?
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for PostgresCSVStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    match &self.rowbuf[ridx][cidx][..] {
                        "" => None,
                        v => Some(v.parse().map_err(|_| {
                            ConnectorXError::cannot_produce::<$t>(Some(self.rowbuf[ridx][cidx].into()))
                        })?),
                    }
                }
            }
        )+
    };
}

impl_csv_produce!(i8, i16, i32, i64, f32, f64, Decimal, Uuid,);

macro_rules! impl_csv_vec_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, Vec<$t>> for PostgresCSVStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&mut self) -> Vec<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let s = &self.rowbuf[ridx][cidx][..];
                    match s {
                        "{}" => vec![],
                        _ if s.len() < 3 => throw!(ConnectorXError::cannot_produce::<$t>(Some(s.into()))),
                        s => s[1..s.len() - 1]
                            .split(",")
                            .map(|v| {
                                v.parse()
                                    .map_err(|_| ConnectorXError::cannot_produce::<$t>(Some(s.into())))
                            })
                            .collect::<Result<Vec<$t>, ConnectorXError>>()?,
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<Vec<$t>>> for PostgresCSVStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&mut self) -> Option<Vec<$t>> {
                    let (ridx, cidx) = self.next_loc()?;
                    let s = &self.rowbuf[ridx][cidx][..];
                    match s {
                        "" => None,
                        "{}" => Some(vec![]),
                        _ if s.len() < 3 => throw!(ConnectorXError::cannot_produce::<$t>(Some(s.into()))),
                        s => Some(
                            s[1..s.len() - 1]
                                .split(",")
                                .map(|v| {
                                    v.parse()
                                        .map_err(|_| ConnectorXError::cannot_produce::<$t>(Some(s.into())))
                                })
                                .collect::<Result<Vec<$t>, ConnectorXError>>()?,
                        ),
                    }
                }
            }
        )+
    };
}

impl_csv_vec_produce!(i8, i16, i32, i64, f32, f64, Decimal, String,);

impl<'r, 'a> Produce<'r, HashMap<String, Option<String>>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;
    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> HashMap<String, Option<String>> {
        unimplemented!("Please use `cursor` protocol for hstore type");
    }
}

impl<'r, 'a> Produce<'r, Option<HashMap<String, Option<String>>>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;
    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<HashMap<String, Option<String>>> {
        unimplemented!("Please use `cursor` protocol for hstore type");
    }
}

impl<'r, 'a> Produce<'r, bool> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> bool {
        let (ridx, cidx) = self.next_loc()?;
        let ret = match &self.rowbuf[ridx][cidx][..] {
            "t" => true,
            "f" => false,
            _ => throw!(ConnectorXError::cannot_produce::<bool>(Some(
                self.rowbuf[ridx][cidx].into()
            ))),
        };
        ret
    }
}

impl<'r, 'a> Produce<'r, Option<bool>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<bool> {
        let (ridx, cidx) = self.next_loc()?;
        let ret = match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            "t" => Some(true),
            "f" => Some(false),
            _ => throw!(ConnectorXError::cannot_produce::<bool>(Some(
                self.rowbuf[ridx][cidx].into()
            ))),
        };
        ret
    }
}

impl<'r, 'a> Produce<'r, Vec<bool>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Vec<bool> {
        let (ridx, cidx) = self.next_loc()?;
        let s = &self.rowbuf[ridx][cidx][..];
        match s {
            "{}" => vec![],
            _ if s.len() < 3 => throw!(ConnectorXError::cannot_produce::<bool>(Some(s.into()))),
            s => s[1..s.len() - 1]
                .split(',')
                .map(|v| match v {
                    "t" => Ok(true),
                    "f" => Ok(false),
                    _ => throw!(ConnectorXError::cannot_produce::<bool>(Some(s.into()))),
                })
                .collect::<Result<Vec<bool>, ConnectorXError>>()?,
        }
    }
}

impl<'r, 'a> Produce<'r, Option<Vec<bool>>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<Vec<bool>> {
        let (ridx, cidx) = self.next_loc()?;
        let s = &self.rowbuf[ridx][cidx][..];
        match s {
            "" => None,
            "{}" => Some(vec![]),
            _ if s.len() < 3 => throw!(ConnectorXError::cannot_produce::<bool>(Some(s.into()))),
            s => Some(
                s[1..s.len() - 1]
                    .split(',')
                    .map(|v| match v {
                        "t" => Ok(true),
                        "f" => Ok(false),
                        _ => throw!(ConnectorXError::cannot_produce::<bool>(Some(s.into()))),
                    })
                    .collect::<Result<Vec<bool>, ConnectorXError>>()?,
            ),
        }
    }
}

impl<'r, 'a> Produce<'r, DateTime<Utc>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> DateTime<Utc> {
        let (ridx, cidx) = self.next_loc()?;
        let s: &str = &self.rowbuf[ridx][cidx][..];
        // postgres csv return example: 1970-01-01 00:00:01+00
        format!("{}:00", s).parse().map_err(|_| {
            ConnectorXError::cannot_produce::<DateTime<Utc>>(Some(self.rowbuf[ridx][cidx].into()))
        })?
    }
}

impl<'r, 'a> Produce<'r, Option<DateTime<Utc>>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<DateTime<Utc>> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => {
                // postgres csv return example: 1970-01-01 00:00:01+00
                Some(format!("{}:00", v).parse().map_err(|_| {
                    ConnectorXError::cannot_produce::<DateTime<Utc>>(Some(v.into()))
                })?)
            }
        }
    }
}

impl<'r, 'a> Produce<'r, NaiveDate> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> NaiveDate {
        let (ridx, cidx) = self.next_loc()?;
        NaiveDate::parse_from_str(&self.rowbuf[ridx][cidx], "%Y-%m-%d").map_err(|_| {
            ConnectorXError::cannot_produce::<NaiveDate>(Some(self.rowbuf[ridx][cidx].into()))
        })?
    }
}

impl<'r, 'a> Produce<'r, Option<NaiveDate>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<NaiveDate> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => Some(
                NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .map_err(|_| ConnectorXError::cannot_produce::<NaiveDate>(Some(v.into())))?,
            ),
        }
    }
}

impl<'r, 'a> Produce<'r, NaiveDateTime> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> NaiveDateTime {
        let (ridx, cidx) = self.next_loc()?;
        NaiveDateTime::parse_from_str(&self.rowbuf[ridx][cidx], "%Y-%m-%d %H:%M:%S").map_err(
            |_| {
                ConnectorXError::cannot_produce::<NaiveDateTime>(Some(
                    self.rowbuf[ridx][cidx].into(),
                ))
            },
        )?
    }
}

impl<'r, 'a> Produce<'r, Option<NaiveDateTime>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<NaiveDateTime> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => Some(
                NaiveDateTime::parse_from_str(v, "%Y-%m-%d %H:%M:%S").map_err(|_| {
                    ConnectorXError::cannot_produce::<NaiveDateTime>(Some(v.into()))
                })?,
            ),
        }
    }
}

impl<'r, 'a> Produce<'r, NaiveTime> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> NaiveTime {
        let (ridx, cidx) = self.next_loc()?;
        NaiveTime::parse_from_str(&self.rowbuf[ridx][cidx], "%H:%M:%S").map_err(|_| {
            ConnectorXError::cannot_produce::<NaiveTime>(Some(self.rowbuf[ridx][cidx].into()))
        })?
    }
}

impl<'r, 'a> Produce<'r, Option<NaiveTime>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&mut self) -> Option<NaiveTime> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => Some(
                NaiveTime::parse_from_str(v, "%H:%M:%S")
                    .map_err(|_| ConnectorXError::cannot_produce::<NaiveTime>(Some(v.into())))?,
            ),
        }
    }
}

impl<'r, 'a> Produce<'r, &'r str> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> &'r str {
        let (ridx, cidx) = self.next_loc()?;
        &self.rowbuf[ridx][cidx]
    }
}

impl<'r, 'a> Produce<'r, Option<&'r str>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<&'r str> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => Some(v),
        }
    }
}

impl<'r, 'a> Produce<'r, Vec<u8>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Vec<u8> {
        let (ridx, cidx) = self.next_loc()?;
        decode(&self.rowbuf[ridx][cidx][2..])? // escape \x in the beginning
    }
}

impl<'r, 'a> Produce<'r, Option<Vec<u8>>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<Vec<u8>> {
        let (ridx, cidx) = self.next_loc()?;
        match &self.rowbuf[ridx][cidx] {
            // escape \x in the beginning, empty if None
            "" => None,
            v => Some(decode(&v[2..])?),
        }
    }
}

impl<'r, 'a> Produce<'r, Value> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Value {
        let (ridx, cidx) = self.next_loc()?;
        let v = &self.rowbuf[ridx][cidx];
        from_str(v).map_err(|_| ConnectorXError::cannot_produce::<Value>(Some(v.into())))?
    }
}

impl<'r, 'a> Produce<'r, Option<Value>> for PostgresCSVStream<'a> {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<Value> {
        let (ridx, cidx) = self.next_loc()?;

        match &self.rowbuf[ridx][cidx][..] {
            "" => None,
            v => {
                from_str(v).map_err(|_| ConnectorXError::cannot_produce::<Value>(Some(v.into())))?
            }
        }
    }
}

pub struct PostgresRawStream<'a> {
    iter: RowIter<'a>,
    rowbuf: Vec<Row>,
    ncols: usize,
    current_col: usize,
    current_row: usize,
    is_finished: bool,
}

impl<'a> PostgresRawStream<'a> {
    pub fn new(iter: RowIter<'a>, schema: &Schema<PostgresTypeSystem>) -> Self {
        Self {
            iter,
            rowbuf: Vec::with_capacity(DB_BUFFER_SIZE),
            ncols: schema.len(),
            current_row: 0,
            current_col: 0,
            is_finished: false,
        }
    }

    #[throws(PostgresSourceError)]
    fn next_loc(&mut self) -> (usize, usize) {
        let ret = (self.current_row, self.current_col);
        self.current_row += (self.current_col + 1) / self.ncols;
        self.current_col = (self.current_col + 1) % self.ncols;
        ret
    }
}

impl<'a> ValueStream<'a> for PostgresRawStream<'a> {
    type TypeSystem = PostgresTypeSystem;
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn fetch_batch(&mut self) -> (usize, bool) {
        assert!(self.current_col == 0);
        if self.is_finished {
            return (0, true);
        }

        let remaining_rows = self.rowbuf.len() - self.current_row;
        if remaining_rows > 0 {
            return (remaining_rows, self.is_finished);
        } else if self.is_finished {
            return (0, self.is_finished);
        }

        if !self.rowbuf.is_empty() {
            self.rowbuf.drain(..);
        }
        for _ in 0..DB_BUFFER_SIZE {
            if let Some(row) = self.iter.next()? {
                self.rowbuf.push(row);
            } else {
                self.is_finished = true;
                break;
            }
        }
        self.current_row = 0;
        self.current_col = 0;
        (self.rowbuf.len(), self.is_finished)
    }
}

macro_rules! impl_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for PostgresRawStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let row = &self.rowbuf[ridx];
                    let val = row.try_get(cidx)?;
                    val
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for PostgresRawStream<'a> {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let row = &self.rowbuf[ridx];
                    let val = row.try_get(cidx)?;
                    val
                }
            }
        )+
    };
}

impl_produce!(
    i8,
    i16,
    i32,
    i64,
    f32,
    f64,
    Decimal,
    Vec<i16>,
    Vec<i32>,
    Vec<i64>,
    Vec<f32>,
    Vec<f64>,
    Vec<Decimal>,
    bool,
    Vec<bool>,
    &'r str,
    Vec<u8>,
    NaiveTime,
    NaiveDateTime,
    DateTime<Utc>,
    NaiveDate,
    Uuid,
    Value,
    HashMap<String, Option<String>>,
    Vec<String>,
);

impl<C> SourceReader for PostgresReader<SimpleProtocol, C>
where
    C: MakeTlsConnect<Socket> + Clone + 'static + Sync + Send,
    C::TlsConnect: Send,
    C::Stream: Send,
    <C::TlsConnect as TlsConnect<Socket>>::Future: Send,
{
    type TypeSystem = PostgresTypeSystem;
    type Stream<'a> = PostgresSimpleStream;
    type Error = PostgresSourceError;

    fn fetch_until_schema(&mut self) -> Result<Schema<Self::TypeSystem>, Self::Error> {
        self.fetch_metadata_generic()
    }

    #[throws(PostgresSourceError)]
    fn value_stream(&mut self, schema: &Schema<PostgresTypeSystem>) -> Self::Stream<'_> {
        let rows = self.conn.simple_query(self.query.as_str())?; // unless reading the data, it seems like issue the query is fast
        PostgresSimpleStream::new(rows, schema)
    }
}

pub struct PostgresSimpleStream {
    rows: Vec<SimpleQueryMessage>,
    ncols: usize,
    current_col: usize,
    current_row: usize,
}
impl PostgresSimpleStream {
    pub fn new(rows: Vec<SimpleQueryMessage>, schema: &Schema<PostgresTypeSystem>) -> Self {
        Self {
            rows,
            ncols: schema.len(),
            current_row: 0,
            current_col: 0,
        }
    }

    #[throws(PostgresSourceError)]
    fn next_loc(&mut self) -> (usize, usize) {
        let ret = (self.current_row, self.current_col);
        self.current_row += (self.current_col + 1) / self.ncols;
        self.current_col = (self.current_col + 1) % self.ncols;
        ret
    }
}

impl<'a> ValueStream<'a> for PostgresSimpleStream {
    type TypeSystem = PostgresTypeSystem;
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn fetch_batch(&mut self) -> (usize, bool) {
        if (self.current_row, self.current_col) == (0, 0) {
            (self.rows.len() - 1, true) // last message is command complete
        } else {
            (0, true)
        }
    }
}

macro_rules! impl_simple_produce_unimplemented {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> $t {
                   unimplemented!("not implemented!");
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                   unimplemented!("not implemented!");
                }
            }
        )+
    };
}

macro_rules! impl_simple_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r> Produce<'r, $t> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let val = match &self.rows[ridx] {
                        SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                            Some(s) => s
                                .parse()
                                .map_err(|_| ConnectorXError::cannot_produce::<$t>(Some(s.into())))?,
                            None => throw!(anyhow!(
                                "Cannot parse NULL in NOT NULL column."
                            )),
                        },
                        SimpleQueryMessage::CommandComplete(c) => {
                            panic!("get command: {}", c);
                        }
                        _ => {
                            panic!("what?");
                        }
                    };
                    val
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let val = match &self.rows[ridx] {
                        SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                            Some(s) => Some(
                                s.parse()
                                    .map_err(|_| ConnectorXError::cannot_produce::<$t>(Some(s.into())))?,
                            ),
                            None => None,
                        },
                        SimpleQueryMessage::CommandComplete(c) => {
                            panic!("get command: {}", c);
                        }
                        _ => {
                            panic!("what?");
                        }
                    };
                    val
                }
            }
        )+
    };
}

impl_simple_produce!(i8, i16, i32, i64, f32, f64, Decimal, Uuid, bool,);
impl_simple_produce_unimplemented!(
    Value,
    HashMap<String, Option<String>>,);

impl<'r> Produce<'r, &'r str> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> &'r str {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => s,
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<&'r str>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<&'r str> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => row.try_get(cidx)?,
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Vec<u8>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Vec<u8> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => {
                    let mut res = s.chars();
                    res.next();
                    res.next();
                    decode(
                        res.enumerate()
                            .fold(String::new(), |acc, (_i, c)| format!("{}{}", acc, c))
                            .chars()
                            .map(|c| c as u8)
                            .collect::<Vec<u8>>(),
                    )?
                }
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<Vec<u8>>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<Vec<u8>> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => {
                    let mut res = s.chars();
                    res.next();
                    res.next();
                    Some(decode(
                        res.enumerate()
                            .fold(String::new(), |acc, (_i, c)| format!("{}{}", acc, c))
                            .chars()
                            .map(|c| c as u8)
                            .collect::<Vec<u8>>(),
                    )?)
                }
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

fn rem_first_and_last(value: &str) -> &str {
    let mut chars = value.chars();
    chars.next();
    chars.next_back();
    chars.as_str()
}

macro_rules! impl_simple_vec_produce {
    ($($t: ty,)+) => {
        $(
            impl<'r> Produce<'r, Vec<$t>> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Vec<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let val = match &self.rows[ridx] {
                        SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                            Some(s) => match s{
                                "" => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
                                "{}" => vec![],
                                _ => rem_first_and_last(s).split(",").map(|token| token.parse().map_err(|_| ConnectorXError::cannot_produce::<Vec<$t>>(Some(s.into())))).collect::<Result<Vec<$t>, ConnectorXError>>()?
                            },
                            None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
                        },
                        SimpleQueryMessage::CommandComplete(c) => {
                            panic!("get command: {}", c);
                        }
                        _ => {
                            panic!("what?");
                        }
                    };
                    val
                }
            }

            impl<'r, 'a> Produce<'r, Option<Vec<$t>>> for PostgresSimpleStream {
                type Error = PostgresSourceError;

                #[throws(PostgresSourceError)]
                fn produce(&'r mut self) -> Option<Vec<$t>> {
                    let (ridx, cidx) = self.next_loc()?;
                    let val = match &self.rows[ridx] {

                        SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                            Some(s) => match s{
                                "" => None,
                                "{}" => Some(vec![]),
                                _ => Some(rem_first_and_last(s).split(",").map(|token| token.parse().map_err(|_| ConnectorXError::cannot_produce::<Vec<$t>>(Some(s.into())))).collect::<Result<Vec<$t>, ConnectorXError>>()?)
                            },
                            None => None,
                        },

                        SimpleQueryMessage::CommandComplete(c) => {
                            panic!("get command: {}", c);
                        }
                        _ => {
                            panic!("what?");
                        }
                    };
                    val
                }
            }
        )+
    };
}
impl_simple_vec_produce!(i16, i32, i64, f32, f64, Decimal, String,);

impl<'r> Produce<'r, Vec<bool>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Vec<bool> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => match s {
                    "" => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
                    "{}" => vec![],
                    _ => rem_first_and_last(s)
                        .split(',')
                        .map(|token| match token {
                            "t" => Ok(true),
                            "f" => Ok(false),
                            _ => {
                                throw!(ConnectorXError::cannot_produce::<Vec<bool>>(Some(s.into())))
                            }
                        })
                        .collect::<Result<Vec<bool>, ConnectorXError>>()?,
                },
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<Vec<bool>>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<Vec<bool>> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => match s {
                    "" => None,
                    "{}" => Some(vec![]),
                    _ => Some(
                        rem_first_and_last(s)
                            .split(',')
                            .map(|token| match token {
                                "t" => Ok(true),
                                "f" => Ok(false),
                                _ => throw!(ConnectorXError::cannot_produce::<Vec<bool>>(Some(
                                    s.into()
                                ))),
                            })
                            .collect::<Result<Vec<bool>, ConnectorXError>>()?,
                    ),
                },
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, NaiveDate> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> NaiveDate {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map_err(|_| ConnectorXError::cannot_produce::<NaiveDate>(Some(s.into())))?,
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<NaiveDate>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<NaiveDate> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => Some(NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
                    ConnectorXError::cannot_produce::<Option<NaiveDate>>(Some(s.into()))
                })?),
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, NaiveTime> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> NaiveTime {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => NaiveTime::parse_from_str(s, "%H:%M:%S")
                    .map_err(|_| ConnectorXError::cannot_produce::<NaiveTime>(Some(s.into())))?,
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<NaiveTime>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<NaiveTime> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => Some(NaiveTime::parse_from_str(s, "%H:%M:%S").map_err(|_| {
                    ConnectorXError::cannot_produce::<Option<NaiveTime>>(Some(s.into()))
                })?),
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, NaiveDateTime> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> NaiveDateTime {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").map_err(|_| {
                    ConnectorXError::cannot_produce::<NaiveDateTime>(Some(s.into()))
                })?,
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<NaiveDateTime>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<NaiveDateTime> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => Some(
                    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").map_err(|_| {
                        ConnectorXError::cannot_produce::<Option<NaiveDateTime>>(Some(s.into()))
                    })?,
                ),
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, DateTime<Utc>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> DateTime<Utc> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => {
                    let time_string = format!("{}:00", s).to_owned();
                    let slice: &str = &time_string[..];
                    let time: DateTime<FixedOffset> =
                        DateTime::parse_from_str(slice, "%Y-%m-%d %H:%M:%S%:z").unwrap();

                    time.with_timezone(&Utc)
                }
                None => throw!(anyhow!("Cannot parse NULL in non-NULL column.")),
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}

impl<'r> Produce<'r, Option<DateTime<Utc>>> for PostgresSimpleStream {
    type Error = PostgresSourceError;

    #[throws(PostgresSourceError)]
    fn produce(&'r mut self) -> Option<DateTime<Utc>> {
        let (ridx, cidx) = self.next_loc()?;
        let val = match &self.rows[ridx] {
            SimpleQueryMessage::Row(row) => match row.try_get(cidx)? {
                Some(s) => {
                    let time_string = format!("{}:00", s).to_owned();
                    let slice: &str = &time_string[..];
                    let time: DateTime<FixedOffset> =
                        DateTime::parse_from_str(slice, "%Y-%m-%d %H:%M:%S%:z").unwrap();

                    Some(time.with_timezone(&Utc))
                }
                None => None,
            },
            SimpleQueryMessage::CommandComplete(c) => {
                panic!("get command: {}", c);
            }
            _ => {
                panic!("what?");
            }
        };
        val
    }
}
