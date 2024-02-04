//! Source implementation for DuckDB embedded database.

use arrow::record_batch::RecordBatch;
use duckdb::Arrow;
use fehler::throws;

use std::sync::Arc;

use super::api::{Connection, ResultReader, Statement};
use super::errors::ConnectorError;

impl Connection for duckdb::Connection {
    type Stmt<'conn> = DuckDBStatement<'conn>
    where
        Self: 'conn;

    fn prepare<'a>(&'a mut self, query: &str) -> Result<Self::Stmt<'a>, ConnectorError> {
        let stmt = duckdb::Connection::prepare(self, query)?;

        Ok(DuckDBStatement { stmt })
    }
}

pub struct DuckDBStatement<'conn> {
    stmt: duckdb::Statement<'conn>,
}

impl<'conn> Statement<'conn> for DuckDBStatement<'conn> {
    type Params = ();

    type Reader<'stmt> = DuckDBReader<'stmt>
    where
        Self: 'stmt;

    fn start(&mut self, _params: ()) -> Result<Self::Reader<'_>, ConnectorError> {
        let arrow = self.stmt.query_arrow([])?;
        Ok(DuckDBReader { arrow })
    }
}

pub struct DuckDBReader<'stmt> {
    arrow: Arrow<'stmt>,
}

impl<'stmt> ResultReader<'stmt> for DuckDBReader<'stmt> {
    #[throws(ConnectorError)]
    fn get_schema(&mut self) -> Arc<arrow::datatypes::Schema> {
        self.arrow.get_schema()
    }
}

impl<'stmt> Iterator for DuckDBReader<'stmt> {
    type Item = Result<RecordBatch, ConnectorError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.arrow.next().map(Ok)
    }
}
