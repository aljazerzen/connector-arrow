//! This module provides [`dispatcher::Dispatcher`], the core struct in ConnectorX
//! that drives the data loading from a source to a destination.
use crate::{
    data_order::{coordinate, DataOrder},
    destinations::{Destination, PartitionWriter},
    sources::{PartitionParser, Source, SourcePartition},
    sql::CXQuery,
    typesystem::{Schema, Transport},
};
use itertools::zip_eq;
use log::debug;
use rayon::prelude::*;
use std::marker::PhantomData;

/// A dispatcher takes a `S: Source`, a `D: Destination`, a `TP: Transport` and a vector of `queries` as input to
/// load data from `S` to `D` using the queries.
pub struct Dispatcher<'a, S, D, TP> {
    src: S,
    dst: &'a mut D,
    queries: Vec<CXQuery<String>>,
    origin_query: Option<String>,
    _phantom: PhantomData<TP>,
}

pub struct PreparedDispatch<S: Source, D: Destination> {
    pub data_order: DataOrder,
    pub src_partitions: Vec<S::Partition>,
    pub dst_partitions: Vec<D::PartitionWriter>,
    pub src_schema: Schema<S::TypeSystem>,
    pub dst_schema: Schema<D::TypeSystem>,
}

impl<'w, S, D, TP> Dispatcher<'w, S, D, TP>
where
    S: Source,
    D: Destination,
    TP: Transport<TSS = S::TypeSystem, TSD = D::TypeSystem, S = S, D = D>,
{
    /// Create a new dispatcher by providing a source, a destination and the queries.
    pub fn new<Q>(src: S, dst: &'w mut D, queries: &[Q], origin_query: Option<String>) -> Self
    where
        for<'a> &'a Q: Into<CXQuery>,
    {
        Self {
            src,
            dst,
            queries: queries.iter().map(Into::into).collect(),
            origin_query,
            _phantom: PhantomData,
        }
    }

    pub fn prepare(mut self) -> Result<PreparedDispatch<S, D>, TP::Error> {
        debug!("Prepare");
        let data_order = coordinate(S::DATA_ORDERS, D::DATA_ORDERS)?;
        self.src.set_data_order(data_order)?;
        self.src.set_queries(self.queries.as_slice());
        self.src.set_origin_query(self.origin_query);

        debug!("Fetching metadata");
        self.src.fetch_metadata()?;
        let src_schema = self.src.schema();
        let dst_schema = src_schema.convert::<TP::TSD, TP>()?;

        let src_partitions: Vec<S::Partition> = self.src.partition()?;

        self.dst.set_metadata(dst_schema.clone(), data_order)?;

        debug!("Create destination partition");
        let mut dst_partitions = Vec::with_capacity(self.queries.len());
        for _ in 0..self.queries.len() {
            dst_partitions.push(self.dst.allocate_partition()?);
        }

        Ok(PreparedDispatch {
            data_order,
            src_partitions,
            dst_partitions,
            src_schema,
            dst_schema,
        })
    }

    /// Start the data loading process.
    pub fn run(self) -> Result<(), TP::Error> {
        debug!("Run dispatcher");
        let PreparedDispatch {
            data_order,
            src_partitions,
            dst_partitions,
            src_schema,
            dst_schema,
        } = self.prepare()?;

        #[cfg(all(not(feature = "branch"), not(feature = "fptr")))]
        compile_error!("branch or fptr, pick one");

        #[cfg(feature = "branch")]
        let types: Vec<_> = zip_eq(src_schema.types, dst_schema.types).collect();

        debug!("Start writing");
        // parse and write
        dst_partitions
            .into_par_iter()
            .zip_eq(src_partitions)
            .enumerate()
            .try_for_each(|(i, (mut dst, mut src))| -> Result<(), TP::Error> {
                #[cfg(feature = "fptr")]
                let f: Vec<_> = zip_eq(&src_schema.types, &dst_schema.types)
                    .map(|(src_ty, dst_ty)| TP::processor(*src_ty, *dst_ty))
                    .collect::<crate::errors::Result<Vec<_>>>()?;

                let mut parser = src.parser()?;

                match data_order {
                    DataOrder::RowMajor => loop {
                        let (n, is_last) = parser.fetch_next()?;
                        dst.aquire_row(n)?;
                        for _ in 0..n {
                            #[allow(clippy::needless_range_loop)]
                            for col in 0..dst.ncols() {
                                #[cfg(feature = "fptr")]
                                f[col](&mut parser, &mut dst)?;

                                #[cfg(feature = "branch")]
                                {
                                    let (s1, s2) = types[col];
                                    TP::process(s1, s2, &mut parser, &mut dst)?;
                                }
                            }
                        }
                        if is_last {
                            break;
                        }
                    },
                    DataOrder::ColumnMajor => loop {
                        let (n, is_last) = parser.fetch_next()?;
                        dst.aquire_row(n)?;
                        #[allow(clippy::needless_range_loop)]
                        for col in 0..dst.ncols() {
                            for _ in 0..n {
                                #[cfg(feature = "fptr")]
                                f[col](&mut parser, &mut dst)?;
                                #[cfg(feature = "branch")]
                                {
                                    let (s1, s2) = types[col];
                                    TP::process(s1, s2, &mut parser, &mut dst)?;
                                }
                            }
                        }
                        if is_last {
                            break;
                        }
                    },
                }

                debug!("Finalize partition {}", i);
                dst.finalize()?;
                debug!("Partition {} finished", i);
                Ok(())
            })?;

        debug!("Writing finished");

        Ok(())
    }

    /// Only fetch the metadata (header) of the destination.
    pub fn get_meta(&mut self) -> Result<(), TP::Error> {
        let dorder = coordinate(S::DATA_ORDERS, D::DATA_ORDERS)?;
        self.src.set_data_order(dorder)?;
        self.src.set_queries(self.queries.as_slice());
        self.src.set_origin_query(self.origin_query.clone());
        self.src.fetch_metadata()?;
        let src_schema = self.src.schema();
        let dst_schema = src_schema.convert::<TP::TSD, TP>()?;
        self.dst.set_metadata(dst_schema, dorder)?;
        Ok(())
    }
}
