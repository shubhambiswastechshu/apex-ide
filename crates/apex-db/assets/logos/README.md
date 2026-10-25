# Database logos

These SVG files are bundled into `apex-db` with `include_bytes!` and exposed
through `apex_db::logo_svg(id)`. They are static image data only.

## Source and license

Most icons come from [Simple Icons](https://simpleicons.org/) v13.21.0
(`https://cdn.jsdelivr.net/npm/simple-icons@13.21.0/icons/<slug>.svg`).
Simple Icons is released under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).
The only change made to each file is a `fill="#RRGGBB"` attribute on the root
`<svg>` element, so the single-path monochrome icon renders in the brand colour
listed by Simple Icons.

| File | Simple Icons slug |
|------|-------------------|
| sqlite.svg | sqlite |
| postgresql.svg | postgresql |
| mysql.svg | mysql |
| mariadb.svg | mariadb |
| oracle.svg | oracle |
| duckdb.svg | duckdb |
| clickhouse.svg | clickhouse |
| snowflake.svg | snowflake |
| bigquery.svg | googlebigquery |
| redshift.svg | amazonredshift |
| databricks.svg | databricks |
| mongodb.svg | mongodb |
| redis.svg | redis |
| cassandra.svg | apachecassandra |
| elasticsearch.svg | elasticsearch |
| neo4j.svg | neo4j |
| influxdb.svg | influxdb |
| cockroachdb.svg | cockroachlabs |
| supabase.svg | supabase |
| firestore.svg | firebase |
| dynamodb.svg | amazondynamodb |
| hive.svg | apachehive |
| trino.svg | trino |

`mssql.svg` and `cosmosdb.svg` are not in Simple Icons (removed at the brand
owner's request). They are original placeholder monograms drawn for Apex IDE: a
rounded square in the brand colour with stroked letters. They are not the
official logos.

## Trademark disclaimer

All product names, logos and brands are the property of their respective
owners. They are used here only to identify the database a connection points
to. Their use does not imply endorsement by, or affiliation with, the trademark
holders. CC0 covers the SVG files themselves; it does not grant any trademark
rights. Check each brand's guidelines before using these logos outside the IDE.

## Adding a logo

1. Save the SVG here as `<driver id>.svg` (24x24 viewBox, root `fill` set).
2. Add the id to the `logos!` list in `src/logos.rs`.
3. `cargo test -p apex-db` checks that every catalog entry has a valid logo.
