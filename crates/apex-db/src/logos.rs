//! Bundled SVG logos, one per catalog entry. See `assets/logos/README.md`.

macro_rules! logos {
    ($($id:literal),* $(,)?) => {
        /// The bundled SVG logo for a driver id, or `None` if there is none.
        ///
        /// Each file is a 24x24 `viewBox` SVG with its brand colour already applied,
        /// so it can be passed straight to an SVG renderer
        /// (for Slint: `slint::Image::load_from_svg_data`).
        pub fn logo_svg(id: &str) -> Option<&'static [u8]> {
            match id {
                $($id => Some(include_bytes!(concat!("../assets/logos/", $id, ".svg")).as_slice()),)*
                _ => None,
            }
        }

        #[cfg(test)]
        const LOGO_IDS: &[&str] = &[$($id),*];
    };
}

logos!(
    "sqlite",
    "postgresql",
    "mysql",
    "mariadb",
    "mssql",
    "oracle",
    "cockroachdb",
    "supabase",
    "duckdb",
    "clickhouse",
    "snowflake",
    "bigquery",
    "redshift",
    "databricks",
    "hive",
    "trino",
    "mongodb",
    "firestore",
    "cosmosdb",
    "redis",
    "dynamodb",
    "cassandra",
    "neo4j",
    "influxdb",
    "elasticsearch",
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn every_catalog_entry_has_a_valid_logo() {
        for d in catalog() {
            let bytes = logo_svg(d.id).unwrap_or_else(|| panic!("no logo for {}", d.id));
            let svg =
                std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("{} is not UTF-8", d.id));
            let svg = svg.trim();
            assert!(svg.starts_with("<svg"), "{} does not start with <svg", d.id);
            assert!(svg.ends_with("</svg>"), "{} does not end with </svg>", d.id);
            assert!(
                svg.contains(r#"viewBox="0 0 24 24""#),
                "{} has no 24x24 viewBox",
                d.id
            );
            assert!(
                svg.contains(r#"xmlns="http://www.w3.org/2000/svg""#),
                "{} lacks xmlns",
                d.id
            );
            assert!(
                svg.contains(&format!(r##"fill="{}""##, d.brand_color)),
                "{} is not filled with its brand colour {}",
                d.id,
                d.brand_color
            );
            assert!(!svg.contains("<script"), "{} contains a script", d.id);
        }
    }

    #[test]
    fn no_orphan_logos() {
        for id in LOGO_IDS {
            assert!(
                crate::find_driver(id).is_some(),
                "logo {id} has no catalog entry"
            );
        }
        assert_eq!(LOGO_IDS.len(), catalog().len());
    }

    #[test]
    fn unknown_id_has_no_logo() {
        assert!(logo_svg("nope").is_none());
    }
}
