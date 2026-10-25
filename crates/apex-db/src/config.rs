//! Saved connection settings and connection URLs.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, Serializer};
use url::Url;

use crate::catalog::{find_driver, find_driver_by_scheme, DriverInfo};
use crate::error::{DbError, Result};

/// File path that opens a private in-memory database (SQLite convention).
pub const IN_MEMORY_PATH: &str = ":memory:";

/// A password or token held in memory only.
///
/// It has no `Serialize` impl and its `Debug` output is redacted, so it cannot
/// end up in a settings file or a log by accident. Persist it in the OS
/// credential store under the connection's [`SecretRef`] instead.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext. Call only at the point of use (for example, when connecting).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Key under which a connection's secret is kept in an external credential store.
///
/// This is a placeholder reference only; resolving it is the app's job.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// Wraps an existing key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The default key for a connection id: `apex-ide/db/<id>`.
    pub fn for_connection(id: &str) -> Self {
        Self(format!("apex-ide/db/{id}"))
    }

    /// The key as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parts of option names that mark a value as a credential, compared
/// case-insensitively (`password`, `sslpassword`, `api_token`, Mongo's
/// `authMechanismProperties`, which can carry `AWS_SESSION_TOKEN`, ...).
const SENSITIVE_OPTION_PARTS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "credential",
    "apikey",
    "api_key",
    "accesskey",
    "access_key",
    "authmechanismproperties",
];

/// Whether a driver option named `key` holds a credential. Such options are
/// never serialized, never put in a URL and redacted in `Debug` output.
pub fn is_sensitive_option(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_OPTION_PARTS.iter().any(|part| key.contains(part))
}

/// Serializes only the options that are not [sensitive](is_sensitive_option).
fn serialize_public_options<S: Serializer>(
    options: &BTreeMap<String, String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.collect_map(options.iter().filter(|(k, _)| !is_sensitive_option(k)))
}

/// Settings for one database connection, as saved in the project or user settings.
///
/// The plaintext [`password`](Self::password) is never serialized; only
/// [`secret_ref`](Self::secret_ref) is written out. The same goes for
/// [sensitive](is_sensitive_option) entries of [`options`](Self::options):
/// they are skipped when serializing and building URLs, and redacted in
/// `Debug` output.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Stable unique id.
    pub id: String,
    /// Name shown in the database tool window.
    pub name: String,
    /// Catalog id of the driver, for example `postgresql`.
    pub driver_id: String,
    /// Server host name or IP address (server databases).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Server port. `None` means the driver's default port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Database (or catalog / keyspace) name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// User name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// In-memory password. Never serialized.
    #[serde(skip)]
    pub password: Option<Secret>,
    /// Where the password is stored outside the settings file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    /// Driver-specific options (for example `sslmode=require`, `read_only=true`).
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "serialize_public_options"
    )]
    pub options: BTreeMap<String, String>,
    /// Database file for file-based drivers. [`IN_MEMORY_PATH`] opens an in-memory database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
}

impl fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let options: BTreeMap<&str, &str> = self
            .options
            .iter()
            .map(|(k, v)| {
                let shown = if is_sensitive_option(k) { "***" } else { v };
                (k.as_str(), shown)
            })
            .collect();
        f.debug_struct("ConnectionConfig")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("driver_id", &self.driver_id)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password)
            .field("secret_ref", &self.secret_ref)
            .field("options", &options)
            .field("file_path", &self.file_path)
            .finish()
    }
}

/// Returns a new id that is unique within this process and very likely unique across runs.
pub fn new_connection_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("conn-{nanos:x}-{n:x}")
}

impl ConnectionConfig {
    /// A new, empty connection with a fresh [`id`](Self::id).
    pub fn new(name: impl Into<String>, driver_id: impl Into<String>) -> Self {
        Self {
            id: new_connection_id(),
            name: name.into(),
            driver_id: driver_id.into(),
            host: None,
            port: None,
            database: None,
            user: None,
            password: None,
            secret_ref: None,
            options: BTreeMap::new(),
            file_path: None,
        }
    }

    /// A connection to a database file (SQLite, DuckDB). The name is the file name.
    pub fn file(driver_id: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let mut config = Self::new(name, driver_id);
        config.file_path = Some(path);
        config
    }

    /// The catalog entry for [`driver_id`](Self::driver_id).
    pub fn driver(&self) -> Result<&'static DriverInfo> {
        find_driver(&self.driver_id).ok_or_else(|| DbError::UnknownDriver(self.driver_id.clone()))
    }

    /// [`port`](Self::port), falling back to the driver's default port.
    pub fn effective_port(&self) -> Option<u16> {
        self.port
            .or_else(|| self.driver().ok().and_then(|d| d.default_port))
    }

    /// Sets the in-memory password and, if none is set yet, a default [`SecretRef`].
    pub fn set_password(&mut self, password: impl Into<String>) {
        self.password = Some(Secret::new(password));
        if self.secret_ref.is_none() {
            self.secret_ref = Some(SecretRef::for_connection(&self.id));
        }
    }

    /// Checks that the fields the driver needs are present.
    pub fn validate(&self) -> Result<&'static DriverInfo> {
        let driver = self.driver()?;
        if self.id.trim().is_empty() {
            return Err(DbError::InvalidConfig("connection id is empty".into()));
        }
        if driver.file_based {
            match &self.file_path {
                Some(p) if !p.as_os_str().is_empty() => {}
                _ => {
                    return Err(DbError::InvalidConfig(format!(
                        "{} needs a database file path",
                        driver.name
                    )))
                }
            }
        } else if self.host.as_deref().is_none_or(|h| h.trim().is_empty()) {
            return Err(DbError::InvalidConfig(format!(
                "{} needs a host",
                driver.name
            )));
        }
        Ok(driver)
    }

    /// Builds a connection URL such as `postgresql://alice@db.local:5432/shop?sslmode=require`
    /// or `sqlite:///C:/data/app.db`.
    ///
    /// The password and [sensitive](is_sensitive_option) options are never
    /// included. File paths must be absolute.
    pub fn to_url(&self) -> Result<String> {
        let driver = self.validate()?;
        let scheme = driver.url_scheme;
        let mut url = if driver.file_based {
            let path = self.file_path.as_deref().unwrap_or_else(|| Path::new(""));
            if path == Path::new(IN_MEMORY_PATH) {
                Url::parse(&format!("{scheme}:{IN_MEMORY_PATH}"))
                    .map_err(|e| DbError::InvalidUrl(e.to_string()))?
            } else {
                file_url(scheme, path)?
            }
        } else {
            server_url(scheme, self)?
        };
        let public: Vec<(&String, &String)> = self
            .options
            .iter()
            .filter(|(k, _)| !is_sensitive_option(k))
            .collect();
        if !public.is_empty() {
            url.query_pairs_mut().extend_pairs(public);
        }
        Ok(url.into())
    }

    /// Parses a connection URL into a new config with a fresh id.
    ///
    /// A password in the URL, as user info or as a `password=` query
    /// parameter, goes into [`password`](Self::password) (memory only) and a
    /// default [`secret_ref`](Self::secret_ref) is set. Other credential
    /// parameters (see [`is_sensitive_option`]) stay in
    /// [`options`](Self::options) for connecting, but are never persisted.
    pub fn from_url(input: &str) -> Result<Self> {
        let url = Url::parse(input.trim()).map_err(|e| DbError::InvalidUrl(e.to_string()))?;
        let driver = find_driver_by_scheme(url.scheme())
            .ok_or_else(|| DbError::UnknownDriver(url.scheme().to_owned()))?;
        let mut query_password = None;
        let mut options = BTreeMap::new();
        for (k, v) in url.query_pairs() {
            if k.eq_ignore_ascii_case("password") {
                query_password = Some(v.into_owned());
            } else {
                options.insert(k.into_owned(), v.into_owned());
            }
        }

        let mut config = if driver.file_based {
            let path = if url.cannot_be_a_base() && url.path() == IN_MEMORY_PATH {
                PathBuf::from(IN_MEMORY_PATH)
            } else {
                parse_file_path(&url)?
            };
            Self::file(driver.id, path)
        } else {
            let host = url
                .host_str()
                .filter(|h| !h.is_empty())
                .ok_or_else(|| DbError::InvalidUrl(format!("`{input}` has no host")))?;
            let host = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();
            let database = percent_decode(url.path().trim_start_matches('/'))?;
            let mut config = Self::new(String::new(), driver.id);
            config.port = url.port();
            config.database = Some(database).filter(|d| !d.is_empty());
            config.user = Some(percent_decode(url.username())?).filter(|u| !u.is_empty());
            config.name = match &config.database {
                Some(db) => format!("{db}@{host}"),
                None => host.clone(),
            };
            config.host = Some(host);
            if let Some(pw) = url.password() {
                config.set_password(percent_decode(pw)?);
            }
            config
        };
        if let Some(pw) = query_password {
            if config.password.is_none() {
                config.set_password(pw);
            }
        }
        config.options = options;
        Ok(config)
    }
}

fn file_url(scheme: &str, path: &Path) -> Result<Url> {
    let file = Url::from_file_path(path).map_err(|()| {
        DbError::InvalidConfig(format!(
            "database file path must be absolute: {}",
            path.display()
        ))
    })?;
    let rest = file.as_str().strip_prefix("file:").unwrap_or(file.as_str());
    Url::parse(&format!("{scheme}:{rest}")).map_err(|e| DbError::InvalidUrl(e.to_string()))
}

fn parse_file_path(url: &Url) -> Result<PathBuf> {
    let rest = &url.as_str()[url.scheme().len() + 1..];
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    if !rest.starts_with("//") {
        return Err(DbError::InvalidUrl(format!(
            "`{url}` must look like {}:///absolute/path",
            url.scheme()
        )));
    }
    let file =
        Url::parse(&format!("file:{rest}")).map_err(|e| DbError::InvalidUrl(e.to_string()))?;
    file.to_file_path().map_err(|()| {
        DbError::InvalidUrl(format!("`{url}` is not a valid file path on this system"))
    })
}

fn server_url(scheme: &str, config: &ConnectionConfig) -> Result<Url> {
    let bad = |what: &str| DbError::InvalidConfig(format!("cannot put {what} in a URL"));
    let mut url = Url::parse(&format!("{scheme}://placeholder"))
        .map_err(|e| DbError::InvalidUrl(e.to_string()))?;
    let host = config.host.as_deref().unwrap_or_default().trim();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    url.set_host(Some(&host)).map_err(|_| bad("this host"))?;
    url.set_port(config.port).map_err(|()| bad("this port"))?;
    if let Some(user) = config.user.as_deref().filter(|u| !u.is_empty()) {
        url.set_username(user).map_err(|()| bad("this user name"))?;
    }
    if let Some(db) = config.database.as_deref().filter(|d| !d.is_empty()) {
        if db.contains('/') {
            return Err(bad("a database name containing `/`"));
        }
        url.set_path(&format!("/{db}"));
    }
    Ok(url)
}

/// Decodes `%XX` escapes. Fails if the result is not UTF-8.
fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).map_err(|_| DbError::InvalidUrl(format!("`{input}` is not valid UTF-8")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg() -> ConnectionConfig {
        let mut c = ConnectionConfig::new("shop", "postgresql");
        c.host = Some("db.example.com".into());
        c.port = Some(6543);
        c.database = Some("shop".into());
        c.user = Some("alice smith".into());
        c.options.insert("sslmode".into(), "require".into());
        c.options
            .insert("application_name".into(), "Apex IDE".into());
        c
    }

    #[test]
    fn server_url_round_trip() {
        let c = pg();
        let url = c.to_url().expect("url");
        assert_eq!(
            url,
            "postgresql://alice%20smith@db.example.com:6543/shop?application_name=Apex+IDE&sslmode=require"
        );
        let back = ConnectionConfig::from_url(&url).expect("parse");
        assert_eq!(back.driver_id, "postgresql");
        assert_eq!(back.host, c.host);
        assert_eq!(back.port, c.port);
        assert_eq!(back.database, c.database);
        assert_eq!(back.user, c.user);
        assert_eq!(back.options, c.options);
        assert!(back.password.is_none());
        assert_eq!(back.to_url().expect("url again"), url);
    }

    #[test]
    fn url_never_contains_password() {
        let mut c = pg();
        c.set_password("hunter2");
        assert!(!c.to_url().expect("url").contains("hunter2"));
    }

    #[test]
    fn password_in_url_stays_in_memory() {
        let c = ConnectionConfig::from_url("postgres://bob:p%40ss@localhost/app").expect("parse");
        assert_eq!(c.driver_id, "postgresql");
        assert_eq!(c.password.as_ref().map(Secret::expose), Some("p@ss"));
        assert_eq!(c.secret_ref, Some(SecretRef::for_connection(&c.id)));
        assert_eq!(c.port, None);
        assert_eq!(c.effective_port(), Some(5432));
        assert_eq!(c.name, "app@localhost");
    }

    #[test]
    fn ipv6_host_round_trip() {
        let mut c = ConnectionConfig::new("local", "redis");
        c.host = Some("::1".into());
        let url = c.to_url().expect("url");
        assert_eq!(url, "redis://[::1]");
        assert_eq!(
            ConnectionConfig::from_url(&url)
                .expect("parse")
                .host
                .as_deref(),
            Some("::1")
        );
    }

    #[test]
    fn file_url_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("my data.db");
        let mut c = ConnectionConfig::file("sqlite", &path);
        c.options.insert("read_only".into(), "true".into());
        let url = c.to_url().expect("url");
        assert!(
            url.starts_with("sqlite:///") || url.starts_with("sqlite://"),
            "{url}"
        );
        assert!(url.ends_with("my%20data.db?read_only=true"), "{url}");
        let back = ConnectionConfig::from_url(&url).expect("parse");
        assert_eq!(back.driver_id, "sqlite");
        assert_eq!(back.file_path.as_deref(), Some(path.as_path()));
        assert_eq!(back.options, c.options);
        assert_eq!(back.name, "my data.db");
    }

    #[test]
    fn memory_url_round_trip() {
        let c = ConnectionConfig::file("sqlite", IN_MEMORY_PATH);
        assert_eq!(c.to_url().expect("url"), "sqlite::memory:");
        let back = ConnectionConfig::from_url("sqlite::memory:").expect("parse");
        assert_eq!(back.file_path.as_deref(), Some(Path::new(IN_MEMORY_PATH)));
    }

    #[test]
    fn relative_file_path_is_rejected_for_urls() {
        let c = ConnectionConfig::file("sqlite", "relative.db");
        assert!(matches!(c.to_url(), Err(DbError::InvalidConfig(_))));
    }

    #[test]
    fn bad_urls() {
        assert!(matches!(
            ConnectionConfig::from_url("not a url"),
            Err(DbError::InvalidUrl(_))
        ));
        assert!(matches!(
            ConnectionConfig::from_url("gopher://x"),
            Err(DbError::UnknownDriver(s)) if s == "gopher"
        ));
        assert!(matches!(
            ConnectionConfig::from_url("mysql:///db"),
            Err(DbError::InvalidUrl(_))
        ));
        assert!(matches!(
            ConnectionConfig::from_url("sqlite:relative.db"),
            Err(DbError::InvalidUrl(_))
        ));
    }

    #[test]
    fn validate() {
        assert!(ConnectionConfig::new("x", "postgresql").validate().is_err());
        assert!(pg().validate().is_ok());
        assert!(matches!(
            ConnectionConfig::new("x", "sqlite").validate(),
            Err(DbError::InvalidConfig(_))
        ));
        assert!(matches!(
            ConnectionConfig::new("x", "nope").validate(),
            Err(DbError::UnknownDriver(_))
        ));
    }

    #[test]
    fn serialization_never_writes_the_password() {
        let mut c = pg();
        c.set_password("hunter2");
        let json = serde_json::to_string(&c).expect("serialize");
        assert!(!json.contains("hunter2"), "{json}");
        assert!(!json.contains("password"), "{json}");
        assert!(json.contains(&format!("apex-ide/db/{}", c.id)), "{json}");
        let back: ConnectionConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(back.password.is_none());
        assert_eq!(back.secret_ref, c.secret_ref);
        assert_eq!(back.options, c.options);
    }

    #[test]
    fn debug_redacts_the_password() {
        let mut c = pg();
        c.set_password("hunter2");
        let debug = format!("{c:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("Secret(***)"));
    }

    #[test]
    fn credentials_in_query_parameters_never_leak() {
        let c = ConnectionConfig::from_url(
            "postgresql://bob@db/app?password=hunter2&sslpassword=keypass&sslmode=require\
             &authMechanismProperties=AWS_SESSION_TOKEN:tok3n",
        )
        .expect("parse");
        assert_eq!(c.password.as_ref().map(Secret::expose), Some("hunter2"));
        assert!(!c.options.contains_key("password"));
        // Still available for connecting...
        assert_eq!(
            c.options.get("sslpassword").map(String::as_str),
            Some("keypass")
        );
        // ...but never written, put in a URL or logged.
        let json = serde_json::to_string(&c).expect("serialize");
        let url = c.to_url().expect("url");
        let debug = format!("{c:?}");
        for secret in ["hunter2", "keypass", "tok3n"] {
            assert!(!json.contains(secret), "{json}");
            assert!(!url.contains(secret), "{url}");
            assert!(!debug.contains(secret), "{debug}");
        }
        assert!(json.contains("sslmode"), "{json}");
        assert_eq!(url, "postgresql://bob@db/app?sslmode=require");
        assert!(debug.contains("sslpassword"), "{debug}");
    }

    #[test]
    fn sensitive_option_names() {
        for key in [
            "password",
            "PassWord",
            "sslpassword",
            "api_token",
            "Secret",
            "PWD",
        ] {
            assert!(is_sensitive_option(key), "{key}");
        }
        for key in ["sslmode", "read_only", "application_name", "user"] {
            assert!(!is_sensitive_option(key), "{key}");
        }
    }

    #[test]
    fn ids_are_unique() {
        let a = new_connection_id();
        let b = new_connection_id();
        assert_ne!(a, b);
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b%2Fc").expect("ok"), "a b/c");
        assert_eq!(percent_decode("100%").expect("ok"), "100%");
        assert_eq!(percent_decode("%zz").expect("ok"), "%zz");
        assert_eq!(percent_decode("%C3%A9").expect("ok"), "é");
        assert!(percent_decode("%FF").is_err());
    }
}
