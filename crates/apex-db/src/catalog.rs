//! Static catalog of the databases Apex IDE knows about.

use serde::Serialize;

/// Broad family a database belongs to. Used to group drivers in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverCategory {
    /// Row-oriented SQL databases (PostgreSQL, MySQL, ...).
    Relational,
    /// Document stores (MongoDB, Firestore, ...).
    Document,
    /// Key-value stores (Redis, DynamoDB, ...).
    KeyValue,
    /// Wide-column stores (Cassandra, ...).
    WideColumn,
    /// Graph databases (Neo4j, ...).
    Graph,
    /// Time-series databases (InfluxDB, ...).
    TimeSeries,
    /// Warehouses and analytical engines (DuckDB, Snowflake, ...).
    Analytical,
    /// Search engines (Elasticsearch, ...).
    Search,
}

impl DriverCategory {
    /// Every category, in the order the UI should list them.
    pub const ALL: [DriverCategory; 8] = [
        Self::Relational,
        Self::Analytical,
        Self::Document,
        Self::KeyValue,
        Self::WideColumn,
        Self::Graph,
        Self::TimeSeries,
        Self::Search,
    ];

    /// Human-readable label, for example `"Wide-column"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Relational => "Relational",
            Self::Document => "Document",
            Self::KeyValue => "Key-value",
            Self::WideColumn => "Wide-column",
            Self::Graph => "Graph",
            Self::TimeSeries => "Time series",
            Self::Analytical => "Analytical",
            Self::Search => "Search",
        }
    }
}

/// Whether a driver can be used today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DriverStatus {
    /// Implemented and registered in [`DriverRegistry::with_builtin`](crate::DriverRegistry::with_builtin).
    Available,
    /// Listed in the catalog, not implemented yet.
    Planned,
}

/// Static description of one database product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DriverInfo {
    /// Stable id, lowercase ASCII (for example `postgresql`). Also the logo file name.
    pub id: &'static str,
    /// Display name (for example `PostgreSQL`).
    pub name: &'static str,
    /// Database family.
    pub category: DriverCategory,
    /// Port used when a connection does not set one. `None` for file databases.
    pub default_port: Option<u16>,
    /// URL scheme used by [`ConnectionConfig::to_url`](crate::ConnectionConfig::to_url).
    pub url_scheme: &'static str,
    /// Brand colour as `#RRGGBB`.
    pub brand_color: &'static str,
    /// Whether a driver implementation exists.
    pub status: DriverStatus,
    /// `true` when the database is a local file (SQLite, DuckDB) rather than a server.
    pub file_based: bool,
}

impl DriverInfo {
    /// Returns `true` when [`status`](Self::status) is [`DriverStatus::Available`].
    pub fn is_available(&self) -> bool {
        self.status == DriverStatus::Available
    }

    /// The SVG logo for this driver. See [`logo_svg`](crate::logo_svg).
    pub fn logo_svg(&self) -> Option<&'static [u8]> {
        crate::logo_svg(self.id)
    }

    /// Brand colour as `(r, g, b)`, or `None` if [`brand_color`](Self::brand_color) is malformed.
    pub fn brand_rgb(&self) -> Option<(u8, u8, u8)> {
        let hex = self.brand_color.strip_prefix('#')?;
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some((channel(0)?, channel(2)?, channel(4)?))
    }
}

use DriverCategory as C;
use DriverStatus::{Available, Planned};

const fn server(
    id: &'static str,
    name: &'static str,
    category: DriverCategory,
    port: u16,
    url_scheme: &'static str,
    brand_color: &'static str,
) -> DriverInfo {
    DriverInfo {
        id,
        name,
        category,
        default_port: Some(port),
        url_scheme,
        brand_color,
        status: Planned,
        file_based: false,
    }
}

static CATALOG: [DriverInfo; 25] = [
    DriverInfo {
        id: "sqlite",
        name: "SQLite",
        category: C::Relational,
        default_port: None,
        url_scheme: "sqlite",
        brand_color: "#003B57",
        status: Available,
        file_based: true,
    },
    server(
        "postgresql",
        "PostgreSQL",
        C::Relational,
        5432,
        "postgresql",
        "#4169E1",
    ),
    server("mysql", "MySQL", C::Relational, 3306, "mysql", "#4479A1"),
    server(
        "mariadb",
        "MariaDB",
        C::Relational,
        3306,
        "mariadb",
        "#003545",
    ),
    server(
        "mssql",
        "Microsoft SQL Server",
        C::Relational,
        1433,
        "mssql",
        "#CC2927",
    ),
    server("oracle", "Oracle", C::Relational, 1521, "oracle", "#F80000"),
    server(
        "cockroachdb",
        "CockroachDB",
        C::Relational,
        26257,
        "cockroachdb",
        "#6933FF",
    ),
    server(
        "supabase",
        "Supabase",
        C::Relational,
        5432,
        "supabase",
        "#3FCF8E",
    ),
    DriverInfo {
        id: "duckdb",
        name: "DuckDB",
        category: C::Analytical,
        default_port: None,
        url_scheme: "duckdb",
        brand_color: "#FFF000",
        status: Planned,
        file_based: true,
    },
    server(
        "clickhouse",
        "ClickHouse",
        C::Analytical,
        8123,
        "clickhouse",
        "#FFCC01",
    ),
    server(
        "snowflake",
        "Snowflake",
        C::Analytical,
        443,
        "snowflake",
        "#29B5E8",
    ),
    server(
        "bigquery",
        "Google BigQuery",
        C::Analytical,
        443,
        "bigquery",
        "#669DF6",
    ),
    server(
        "redshift",
        "Amazon Redshift",
        C::Analytical,
        5439,
        "redshift",
        "#8C4FFF",
    ),
    server(
        "databricks",
        "Databricks",
        C::Analytical,
        443,
        "databricks",
        "#FF3621",
    ),
    server(
        "hive",
        "Apache Hive",
        C::Analytical,
        10000,
        "hive2",
        "#FDEE21",
    ),
    server(
        "trino",
        "Trino / Presto",
        C::Analytical,
        8080,
        "trino",
        "#DD00A1",
    ),
    server(
        "mongodb",
        "MongoDB",
        C::Document,
        27017,
        "mongodb",
        "#47A248",
    ),
    server(
        "firestore",
        "Firebase Firestore",
        C::Document,
        443,
        "firestore",
        "#DD2C00",
    ),
    server(
        "cosmosdb",
        "Azure Cosmos DB",
        C::Document,
        443,
        "cosmosdb",
        "#0078D4",
    ),
    server("redis", "Redis", C::KeyValue, 6379, "redis", "#FF4438"),
    server(
        "dynamodb",
        "Amazon DynamoDB",
        C::KeyValue,
        443,
        "dynamodb",
        "#4053D6",
    ),
    server(
        "cassandra",
        "Apache Cassandra",
        C::WideColumn,
        9042,
        "cassandra",
        "#1287B1",
    ),
    server("neo4j", "Neo4j", C::Graph, 7687, "neo4j", "#4581C3"),
    server(
        "influxdb",
        "InfluxDB",
        C::TimeSeries,
        8086,
        "influxdb",
        "#22ADF6",
    ),
    server(
        "elasticsearch",
        "Elasticsearch",
        C::Search,
        9200,
        "elasticsearch",
        "#005571",
    ),
];

/// Extra URL schemes accepted when parsing, mapped to a driver id.
const SCHEME_ALIASES: &[(&str, &str)] = &[
    ("postgres", "postgresql"),
    ("sqlserver", "mssql"),
    ("presto", "trino"),
    ("hive", "hive"),
    ("rediss", "redis"),
    ("mongodb+srv", "mongodb"),
    ("cockroach", "cockroachdb"),
];

/// Every database Apex IDE knows about, grouped by category.
pub fn catalog() -> &'static [DriverInfo] {
    &CATALOG
}

/// The SQLite entry (first in the catalog; the constant index is checked at compile time).
pub(crate) fn sqlite_info() -> &'static DriverInfo {
    &CATALOG[0]
}

/// Looks up a driver by its [`id`](DriverInfo::id).
pub fn find_driver(id: &str) -> Option<&'static DriverInfo> {
    CATALOG.iter().find(|d| d.id == id)
}

/// Looks up a driver by URL scheme (case-insensitive), including common aliases
/// such as `postgres` and `mongodb+srv`.
pub fn find_driver_by_scheme(scheme: &str) -> Option<&'static DriverInfo> {
    let scheme = scheme.to_ascii_lowercase();
    CATALOG.iter().find(|d| d.url_scheme == scheme).or_else(|| {
        SCHEME_ALIASES
            .iter()
            .find(|(alias, _)| *alias == scheme)
            .and_then(|(_, id)| find_driver(id))
    })
}

/// Drivers in one category, in catalog order.
pub fn drivers_in(category: DriverCategory) -> impl Iterator<Item = &'static DriverInfo> {
    CATALOG.iter().filter(move |d| d.category == category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_and_schemes_are_unique() {
        let mut ids = HashSet::new();
        let mut schemes = HashSet::new();
        let mut names = HashSet::new();
        for d in catalog() {
            assert!(ids.insert(d.id), "duplicate id {}", d.id);
            assert!(
                schemes.insert(d.url_scheme),
                "duplicate scheme {}",
                d.url_scheme
            );
            assert!(names.insert(d.name), "duplicate name {}", d.name);
        }
    }

    #[test]
    fn ids_are_lowercase_ascii() {
        for d in catalog() {
            assert!(
                d.id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "bad id {}",
                d.id
            );
        }
    }

    #[test]
    fn covers_required_databases() {
        for id in [
            "sqlite",
            "postgresql",
            "mysql",
            "mariadb",
            "mssql",
            "oracle",
            "duckdb",
            "clickhouse",
            "snowflake",
            "bigquery",
            "redshift",
            "databricks",
            "mongodb",
            "redis",
            "cassandra",
            "elasticsearch",
            "neo4j",
            "influxdb",
            "cockroachdb",
            "supabase",
            "firestore",
            "dynamodb",
            "cosmosdb",
            "hive",
            "trino",
        ] {
            assert!(find_driver(id).is_some(), "missing {id}");
        }
    }

    #[test]
    fn brand_colors_parse() {
        for d in catalog() {
            assert!(
                d.brand_rgb().is_some(),
                "bad colour {} for {}",
                d.brand_color,
                d.id
            );
        }
        assert_eq!(
            find_driver("postgresql").and_then(|d| d.brand_rgb()),
            Some((0x41, 0x69, 0xE1))
        );
    }

    #[test]
    fn file_databases_have_no_port() {
        for d in catalog() {
            assert_eq!(d.file_based, d.default_port.is_none(), "{}", d.id);
        }
    }

    #[test]
    fn scheme_lookup_and_aliases() {
        assert_eq!(
            find_driver_by_scheme("postgresql").map(|d| d.id),
            Some("postgresql")
        );
        assert_eq!(
            find_driver_by_scheme("POSTGRES").map(|d| d.id),
            Some("postgresql")
        );
        assert_eq!(
            find_driver_by_scheme("mongodb+srv").map(|d| d.id),
            Some("mongodb")
        );
        assert_eq!(find_driver_by_scheme("hive2").map(|d| d.id), Some("hive"));
        assert!(find_driver_by_scheme("gopher").is_none());
        for (alias, id) in SCHEME_ALIASES {
            assert!(
                find_driver(id).is_some(),
                "alias {alias} points to missing {id}"
            );
        }
    }

    #[test]
    fn every_category_is_used() {
        for c in DriverCategory::ALL {
            assert!(drivers_in(c).next().is_some(), "no driver in {}", c.label());
        }
    }
}
