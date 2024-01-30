#[cfg(feature = "src_mysql")]
use crate::sources::mysql::{BinaryProtocol as MySQLBinaryProtocol, TextProtocol};
#[cfg(feature = "src_postgres")]
use crate::sources::postgres::{
    rewrite_tls_args, BinaryProtocol as PgBinaryProtocol, CSVProtocol, CursorProtocol,
    SimpleProtocol,
};
use crate::{prelude::*, sql::CXQuery};
use fehler::{throw, throws};
use log::debug;
#[cfg(feature = "src_postgres")]
use postgres::NoTls;
#[cfg(feature = "src_postgres")]
use postgres_openssl::MakeTlsConnector;
#[allow(unused_imports)]
use std::sync::Arc;

#[allow(unreachable_code, unreachable_patterns, unused_variables, unused_mut)]
#[throws(ConnectorXOutError)]
pub fn get_arrow(source_conn: &SourceConn, queries: &[CXQuery<String>]) -> ArrowDestination {
    let mut destination = ArrowDestination::new();
    let protocol = source_conn.proto.as_str();
    debug!("Protocol: {}", protocol);

    match source_conn.ty {
        #[cfg(feature = "src_postgres")]
        SourceType::Postgres => {
            let (config, tls) = rewrite_tls_args(&source_conn.conn)?;
            match (protocol, tls) {
                ("csv", Some(tls_conn)) => {
                    let source = PostgresSource::<CSVProtocol, MakeTlsConnector>::new(
                        config,
                        tls_conn,
                        queries.len(),
                    )?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<CSVProtocol, MakeTlsConnector>,
                    >::new(source, &mut destination, queries);
                    dispatcher.run()?;
                }
                ("csv", None) => {
                    let source =
                        PostgresSource::<CSVProtocol, NoTls>::new(config, NoTls, queries.len())?;
                    let dispatcher =
                        Dispatcher::<_, _, PostgresArrowTransport<CSVProtocol, NoTls>>::new(
                            source,
                            &mut destination,
                            queries,
                        );
                    dispatcher.run()?;
                }
                ("binary", Some(tls_conn)) => {
                    let source = PostgresSource::<PgBinaryProtocol, MakeTlsConnector>::new(
                        config,
                        tls_conn,
                        queries.len(),
                    )?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<PgBinaryProtocol, MakeTlsConnector>,
                    >::new(source, &mut destination, queries);
                    dispatcher.run()?;
                }
                ("binary", None) => {
                    let source = PostgresSource::<PgBinaryProtocol, NoTls>::new(
                        config,
                        NoTls,
                        queries.len(),
                    )?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<PgBinaryProtocol, NoTls>,
                    >::new(source, &mut destination, queries);
                    dispatcher.run()?;
                }
                ("cursor", Some(tls_conn)) => {
                    let source = PostgresSource::<CursorProtocol, MakeTlsConnector>::new(
                        config,
                        tls_conn,
                        queries.len(),
                    )?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<CursorProtocol, MakeTlsConnector>,
                    >::new(source, &mut destination, queries);
                    dispatcher.run()?;
                }
                ("cursor", None) => {
                    let source =
                        PostgresSource::<CursorProtocol, NoTls>::new(config, NoTls, queries.len())?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<CursorProtocol, NoTls>,
                    >::new(source, &mut destination, queries);
                    dispatcher.run()?;
                }
                ("simple", Some(tls_conn)) => {
                    let sb = PostgresSource::<SimpleProtocol, MakeTlsConnector>::new(
                        config,
                        tls_conn,
                        queries.len(),
                    )?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<SimpleProtocol, MakeTlsConnector>,
                    >::new(sb, &mut destination, queries);
                    debug!("Running dispatcher");
                    dispatcher.run()?;
                }
                ("simple", None) => {
                    let sb =
                        PostgresSource::<SimpleProtocol, NoTls>::new(config, NoTls, queries.len())?;
                    let dispatcher = Dispatcher::<
                        _,
                        _,
                        PostgresArrowTransport<SimpleProtocol, NoTls>,
                    >::new(sb, &mut destination, queries);
                    debug!("Running dispatcher");
                    dispatcher.run()?;
                }
                _ => unimplemented!("{} protocol not supported", protocol),
            }
        }
        #[cfg(feature = "src_mysql")]
        SourceType::MySQL => match protocol {
            "binary" => {
                let source =
                    MySQLSource::<MySQLBinaryProtocol>::new(&source_conn.conn[..], queries.len())?;
                let dispatcher = Dispatcher::<_, _, MySQLArrowTransport<MySQLBinaryProtocol>>::new(
                    source,
                    &mut destination,
                    queries,
                );
                dispatcher.run()?;
            }
            "text" => {
                let source =
                    MySQLSource::<TextProtocol>::new(&source_conn.conn[..], queries.len())?;
                let dispatcher = Dispatcher::<_, _, MySQLArrowTransport<TextProtocol>>::new(
                    source,
                    &mut destination,
                    queries,
                );
                dispatcher.run()?;
            }
            _ => unimplemented!("{} protocol not supported", protocol),
        },
        #[cfg(feature = "src_sqlite")]
        SourceType::SQLite => {
            // remove the first "sqlite://" manually since url.path is not correct for windows
            let path = &source_conn.conn.as_str()[9..];
            let source = SQLiteSource::new(path, queries.len())?;
            let dispatcher =
                Dispatcher::<_, _, SQLiteArrowTransport>::new(source, &mut destination, queries);
            dispatcher.run()?;
        }
        #[cfg(feature = "src_mssql")]
        SourceType::MsSQL => {
            let rt = Arc::new(tokio::runtime::Runtime::new().expect("Failed to create runtime"));
            let source = MsSQLSource::new(rt, &source_conn.conn[..], queries.len())?;
            let dispatcher =
                Dispatcher::<_, _, MsSQLArrowTransport>::new(source, &mut destination, queries);
            dispatcher.run()?;
        }
        #[cfg(feature = "src_oracle")]
        SourceType::Oracle => {
            let source = OracleSource::new(&source_conn.conn[..], queries.len())?;
            let dispatcher =
                Dispatcher::<_, _, OracleArrowTransport>::new(source, &mut destination, queries);
            dispatcher.run()?;
        }
        #[cfg(feature = "src_bigquery")]
        SourceType::BigQuery => {
            let rt = Arc::new(tokio::runtime::Runtime::new().expect("Failed to create runtime"));
            let source = BigQuerySource::new(rt, &source_conn.conn[..])?;
            let dispatcher =
                Dispatcher::<_, _, BigQueryArrowTransport>::new(source, &mut destination, queries);
            dispatcher.run()?;
        }
        _ => throw!(ConnectorXOutError::SourceNotSupport(format!(
            "{:?}",
            source_conn.ty
        ))),
    }

    destination
}
