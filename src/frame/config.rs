//! Layered config resolution: CLI flag > `DBOPS_<DB>_<FIELD>` env var >
//! config file value > built-in default.
//!
//! Backed by an optional TOML file (`~/.dbops.toml` by default, or an
//! explicit path) holding named connection profiles plus a `[safety]` list
//! of profile names that destructive commands should refuse to touch
//! without extra confirmation. A missing config file is not an error —
//! every field can also come from an env var or (eventually) a CLI flag.

// `resolve()` isn't threaded into `main.rs`/`Ctx` yet -- that lands with the
// task that wires `ResolvedProfile` into the domain modules (see
// `frame::ctx::Ctx`, which has the same `#[allow(dead_code)]` for the same
// reason). Until then everything below is only reachable from this module's
// own tests, which trips `dead_code` on every item transitively.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::frame::cli::Cli;
use crate::frame::secret::{Secret, SecretRef};

/// `DBOPS_<DB>_<FIELD>` env var names, kept as named constants so the
/// mapping stays obviously in sync with the merge logic in [`resolve`].
mod env_keys {
    pub const PROFILE: &str = "DBOPS_PROFILE";
    pub const OS_HOSTS: &str = "DBOPS_OS_HOSTS";
    pub const OS_USERNAME: &str = "DBOPS_OS_USERNAME";
    pub const OS_PASSWORD: &str = "DBOPS_OS_PASSWORD";
    pub const MONGO_URI: &str = "DBOPS_MONGO_URI";
    pub const PG_HOST: &str = "DBOPS_PG_HOST";
    pub const PG_PORT: &str = "DBOPS_PG_PORT";
    pub const PG_USER: &str = "DBOPS_PG_USER";
    pub const PG_PASSWORD: &str = "DBOPS_PG_PASSWORD";
    pub const PG_DBNAME: &str = "DBOPS_PG_DBNAME";
    pub const REDIS_URI: &str = "DBOPS_REDIS_URI";
}

// --- resolved output -------------------------------------------------------

/// One resolved connection profile: config-file values merged with
/// `DBOPS_*` env overrides (and, eventually, per-field CLI flags), with
/// secrets already resolved via [`SecretRef`] and wrapped in [`Secret`] so
/// they never leak through `Debug`.
#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
    pub name: String,
    /// `true` if `name` is listed under `[safety] protected_profiles` in
    /// the config file — destructive domain commands should use this to
    /// require extra confirmation.
    pub protected: bool,
    pub opensearch: OpenSearchProfile,
    pub mongodb: MongoProfile,
    pub postgres: PostgresProfile,
    pub redis: RedisProfile,
}

#[derive(Debug, Clone, Default)]
pub struct OpenSearchProfile {
    pub hosts: Vec<String>,
    pub username: Option<Secret>,
    pub password: Option<Secret>,
}

#[derive(Debug, Clone, Default)]
pub struct MongoProfile {
    pub uri: Option<Secret>,
}

#[derive(Debug, Clone, Default)]
pub struct PostgresProfile {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<Secret>,
    pub password: Option<Secret>,
    pub dbname: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RedisProfile {
    pub uri: Option<Secret>,
}

// --- TOML file schema (raw, pre-merge) --------------------------------------

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    default_profile: Option<String>,
    #[serde(default)]
    profiles: HashMap<String, FileProfile>,
    #[serde(default)]
    safety: SafetyConfig,
}

#[derive(Debug, Deserialize, Default)]
struct SafetyConfig {
    #[serde(default)]
    protected_profiles: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileProfile {
    opensearch: Option<FileOpenSearch>,
    mongodb: Option<FileMongo>,
    postgres: Option<FilePostgres>,
    redis: Option<FileRedis>,
}

#[derive(Debug, Deserialize, Default)]
struct FileOpenSearch {
    #[serde(default)]
    hosts: Vec<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileMongo {
    uri: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FilePostgres {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    dbname: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FileRedis {
    uri: Option<String>,
}

// --- resolution --------------------------------------------------------------

/// Resolve a connection profile from CLI flags, `DBOPS_*` env vars, and an
/// optional TOML config file, in that priority order.
///
/// - `env` is an explicit snapshot rather than a direct `std::env` read, so
///   resolution stays deterministic and unit-testable without mutating
///   global process state. Real callers pass `std::env::vars().collect()`.
/// - `config_path` is the config file path already resolved by the caller
///   (typically `cli.config.clone()`), or `None` to fall back to
///   `~/.dbops.toml`. A missing file at the fallback location is not an
///   error; a missing file at an explicitly given path is.
pub fn resolve(
    cli: &Cli,
    env: &HashMap<String, String>,
    config_path: Option<&Path>,
) -> Result<ResolvedProfile> {
    let loaded = load_file_config(config_path)?;
    let file = &loaded.data;

    let profile_name = pick(
        cli.profile.as_deref(),
        env.get(env_keys::PROFILE).map(String::as_str),
        file.default_profile.as_deref(),
    )
    .unwrap_or("default")
    .to_string();

    let file_profile = file.profiles.get(&profile_name);
    if loaded.file_present && !file.profiles.is_empty() && file_profile.is_none() {
        let available: Vec<&str> = file.profiles.keys().map(String::as_str).collect();
        anyhow::bail!(
            "profile '{profile_name}' not found in config (available: {})",
            available.join(", ")
        );
    }

    let protected = file
        .safety
        .protected_profiles
        .iter()
        .any(|p| p == &profile_name);

    let os_file = file_profile.and_then(|p| p.opensearch.as_ref());
    let opensearch = OpenSearchProfile {
        hosts: pick_vec(
            env.get(env_keys::OS_HOSTS).map(String::as_str),
            os_file.map(|o| o.hosts.as_slice()),
        ),
        username: pick_secret(
            None,
            env.get(env_keys::OS_USERNAME).map(String::as_str),
            os_file.and_then(|o| o.username.as_deref()),
        )?,
        password: pick_secret(
            None,
            env.get(env_keys::OS_PASSWORD).map(String::as_str),
            os_file.and_then(|o| o.password.as_deref()),
        )?,
    };

    let mongo_file = file_profile.and_then(|p| p.mongodb.as_ref());
    let mongodb = MongoProfile {
        uri: pick_secret(
            None,
            env.get(env_keys::MONGO_URI).map(String::as_str),
            mongo_file.and_then(|m| m.uri.as_deref()),
        )?,
    };

    let pg_file = file_profile.and_then(|p| p.postgres.as_ref());
    let postgres = PostgresProfile {
        host: pick_owned(
            None,
            env.get(env_keys::PG_HOST).map(String::as_str),
            pg_file.and_then(|p| p.host.as_deref()),
        ),
        port: pick_port(
            None,
            env.get(env_keys::PG_PORT).map(String::as_str),
            pg_file.and_then(|p| p.port),
        )?,
        user: pick_secret(
            None,
            env.get(env_keys::PG_USER).map(String::as_str),
            pg_file.and_then(|p| p.user.as_deref()),
        )?,
        password: pick_secret(
            None,
            env.get(env_keys::PG_PASSWORD).map(String::as_str),
            pg_file.and_then(|p| p.password.as_deref()),
        )?,
        dbname: pick_owned(
            None,
            env.get(env_keys::PG_DBNAME).map(String::as_str),
            pg_file.and_then(|p| p.dbname.as_deref()),
        ),
    };

    let redis_file = file_profile.and_then(|p| p.redis.as_ref());
    let redis = RedisProfile {
        uri: pick_secret(
            None,
            env.get(env_keys::REDIS_URI).map(String::as_str),
            redis_file.and_then(|r| r.uri.as_deref()),
        )?,
    };

    Ok(ResolvedProfile {
        name: profile_name,
        protected,
        opensearch,
        mongodb,
        postgres,
        redis,
    })
}

/// Priority merge for a single field: CLI flag, then env var, then config
/// file value. `flag` is `None` at every call site in [`resolve`] today —
/// the global `Cli` struct (`frame::cli::Cli`) doesn't expose per-database
/// flags yet — but every field already merges through this function, so
/// wiring up a real flag later is a one-line change at the call site, not a
/// redesign of the merge order.
fn pick<'a>(
    flag: Option<&'a str>,
    env: Option<&'a str>,
    config: Option<&'a str>,
) -> Option<&'a str> {
    flag.or(env).or(config)
}

fn pick_owned(flag: Option<&str>, env: Option<&str>, config: Option<&str>) -> Option<String> {
    pick(flag, env, config).map(str::to_string)
}

fn pick_secret(
    flag: Option<&str>,
    env: Option<&str>,
    config: Option<&str>,
) -> Result<Option<Secret>> {
    match pick(flag, env, config) {
        Some(raw) => Ok(Some(Secret::from(SecretRef::parse(raw).resolve()?))),
        None => Ok(None),
    }
}

fn pick_port(flag: Option<u16>, env: Option<&str>, config: Option<u16>) -> Result<Option<u16>> {
    if flag.is_some() {
        return Ok(flag);
    }
    if let Some(raw) = env {
        return raw
            .parse::<u16>()
            .map(Some)
            .with_context(|| format!("invalid port in {}: {raw}", env_keys::PG_PORT));
    }
    Ok(config)
}

/// Comma-separated in the env var, a native TOML array in the config file.
fn pick_vec(env: Option<&str>, config: Option<&[String]>) -> Vec<String> {
    if let Some(raw) = env {
        return raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    config.map(<[String]>::to_vec).unwrap_or_default()
}

// --- file loading --------------------------------------------------------------

struct LoadedConfig {
    data: FileConfig,
    /// Whether an actual file was found and parsed, as opposed to falling
    /// back to an empty default because no config file exists anywhere.
    /// Used to decide whether an unknown profile name is a hard error (the
    /// user clearly maintains a profiles table) or just "no config, fields
    /// come from env/flags instead".
    file_present: bool,
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".dbops.toml"))
}

fn load_file_config(explicit_path: Option<&Path>) -> Result<LoadedConfig> {
    let path = match explicit_path {
        Some(p) => p.to_path_buf(),
        None => match default_config_path() {
            Some(p) if p.exists() => p,
            _ => {
                return Ok(LoadedConfig {
                    data: FileConfig::default(),
                    file_present: false,
                })
            }
        },
    };

    if !path.exists() {
        if explicit_path.is_some() {
            anyhow::bail!("config file not found: {}", path.display());
        }
        return Ok(LoadedConfig {
            data: FileConfig::default(),
            file_present: false,
        });
    }

    if let Some(warning) = permission_warning(&path) {
        eprintln!("warning: {warning}");
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let data: FileConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config file: {}", path.display()))?;
    Ok(LoadedConfig {
        data,
        file_present: true,
    })
}

/// Returns a warning message if the config file at `path` is readable or
/// writable by group/other (i.e. its mode is not `0600`). Kept pure (no
/// I/O side effect beyond reading metadata) so it's unit-testable; the
/// caller decides how to surface the message.
#[cfg(unix)]
fn permission_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).ok()?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        Some(format!(
            "config file {} is readable/writable by group or other (mode {mode:o}); it may contain secrets \
             — run `chmod 600 {}`",
            path.display(),
            path.display()
        ))
    } else {
        None
    }
}

#[cfg(not(unix))]
fn permission_warning(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::frame::cli::Commands;
    use crate::sys::{SysArgs, SysCommand};

    fn test_cli(profile: Option<&str>) -> Cli {
        Cli {
            command: Commands::Sys(SysArgs {
                command: SysCommand::Check,
            }),
            profile: profile.map(str::to_string),
            config: None,
            json: false,
            timeout: None,
            dry_run: false,
            yes: false,
            insecure: false,
            verbose: 0,
        }
    }

    /// Unique path per call so parallel `#[test]` threads never collide on
    /// the same temp file (no `tempfile` crate in this workspace).
    fn unique_temp_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "dbops-config-test-{}-{n}-{nanos}.toml",
            std::process::id()
        ))
    }

    fn write_temp_config(contents: &str) -> PathBuf {
        let path = unique_temp_path();
        fs::write(&path, contents).unwrap();
        path
    }

    fn empty_env() -> HashMap<String, String> {
        HashMap::new()
    }

    // --- priority: flag > env > config > default --------------------------

    #[test]
    fn pick_priority_flag_env_config_default() {
        assert_eq!(
            pick(Some("flag"), Some("env"), Some("config")),
            Some("flag")
        );
        assert_eq!(pick(None, Some("env"), Some("config")), Some("env"));
        assert_eq!(pick(None, None, Some("config")), Some("config"));
        assert_eq!(pick(None, None, None), None);
    }

    #[test]
    fn profile_priority_case_1_cli_flag_wins() {
        let path = write_temp_config("default_profile = \"dev\"\n");
        let mut env = empty_env();
        env.insert(env_keys::PROFILE.to_string(), "prod".to_string());

        let cli = test_cli(Some("staging"));
        let profile = resolve(&cli, &env, Some(&path)).unwrap();
        assert_eq!(profile.name, "staging");

        fs::remove_file(path).ok();
    }

    #[test]
    fn profile_priority_case_2_env_wins_over_config() {
        let path = write_temp_config("default_profile = \"dev\"\n");
        let mut env = empty_env();
        env.insert(env_keys::PROFILE.to_string(), "prod".to_string());

        let cli = test_cli(None);
        let profile = resolve(&cli, &env, Some(&path)).unwrap();
        assert_eq!(profile.name, "prod");

        fs::remove_file(path).ok();
    }

    #[test]
    fn profile_priority_case_3_config_wins_over_builtin_default() {
        let path = write_temp_config("default_profile = \"dev\"\n");
        let env = empty_env();

        let cli = test_cli(None);
        let profile = resolve(&cli, &env, Some(&path)).unwrap();
        assert_eq!(profile.name, "dev");

        fs::remove_file(path).ok();
    }

    // --- secret ref resolution end-to-end ----------------------------------

    #[test]
    fn resolves_env_and_cmd_secret_refs_from_config() {
        let path = write_temp_config(
            "[profiles.prod.postgres]\nhost = \"pg.internal\"\nuser = \"env:DBOPS_CFG_TEST_PG_USER\"\n\
             password = \"cmd:printf hunter2\"\n",
        );
        // `env:VAR` inside a config value is a `SecretRef` that reads the
        // real process environment at resolve time (per `SecretRef::resolve`'s
        // contract) -- distinct from the `env: &HashMap` snapshot `resolve()`
        // takes for `DBOPS_<DB>_<FIELD>` overrides.
        //
        // SAFETY: `DBOPS_CFG_TEST_PG_USER` is unique to this test.
        unsafe { std::env::set_var("DBOPS_CFG_TEST_PG_USER", "postgres") };

        let env = empty_env();
        let cli = test_cli(Some("prod"));
        let profile = resolve(&cli, &env, Some(&path)).unwrap();

        assert_eq!(profile.postgres.host.as_deref(), Some("pg.internal"));
        assert_eq!(profile.postgres.user.unwrap().expose(), "postgres");
        assert_eq!(profile.postgres.password.unwrap().expose(), "hunter2");

        unsafe { std::env::remove_var("DBOPS_CFG_TEST_PG_USER") };
        fs::remove_file(path).ok();
    }

    #[test]
    fn db_field_env_var_overrides_config_value() {
        let path = write_temp_config("[profiles.prod.postgres]\nhost = \"config-host\"\n");
        let mut env = empty_env();
        env.insert(env_keys::PG_HOST.to_string(), "env-host".to_string());

        let cli = test_cli(Some("prod"));
        let profile = resolve(&cli, &env, Some(&path)).unwrap();
        assert_eq!(profile.postgres.host.as_deref(), Some("env-host"));

        fs::remove_file(path).ok();
    }

    // --- missing config file is not an error --------------------------------

    #[test]
    fn missing_config_file_is_not_an_error() {
        // No explicit path, and HOME points at an empty temp dir so this
        // doesn't depend on (or clobber) a real ~/.dbops.toml.
        let home = unique_temp_path().with_extension("home-dir");
        fs::create_dir_all(&home).unwrap();
        let prev_home = std::env::var_os("HOME");

        // SAFETY: no other test reads/writes HOME concurrently with this
        // one's narrow set/restore window.
        unsafe { std::env::set_var("HOME", &home) };

        let mut env = empty_env();
        env.insert(env_keys::PG_HOST.to_string(), "from-env-only".to_string());
        let cli = test_cli(None);
        let profile = resolve(&cli, &env, None).unwrap();

        assert_eq!(profile.name, "default");
        assert_eq!(profile.postgres.host.as_deref(), Some("from-env-only"));

        match prev_home {
            Some(val) => unsafe { std::env::set_var("HOME", val) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn explicit_missing_config_path_is_an_error() {
        let cli = test_cli(None);
        let env = empty_env();
        let missing = Path::new("/nonexistent/dbops-config-test-path/dbops.toml");
        let err = resolve(&cli, &env, Some(missing)).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn unknown_profile_name_in_nonempty_config_errors() {
        let path = write_temp_config("[profiles.prod.postgres]\nhost = \"pg.internal\"\n");
        let cli = test_cli(Some("staging"));
        let env = empty_env();
        let err = resolve(&cli, &env, Some(&path)).unwrap_err();
        assert!(err.to_string().contains("not found in config"));

        fs::remove_file(path).ok();
    }

    // --- protected profiles --------------------------------------------------

    #[test]
    fn protected_profile_flag_is_set() {
        let path = write_temp_config(
            "[profiles.prod.postgres]\nhost = \"pg.internal\"\n[safety]\nprotected_profiles = [\"prod\"]\n",
        );
        let cli = test_cli(Some("prod"));
        let env = empty_env();
        let profile = resolve(&cli, &env, Some(&path)).unwrap();
        assert!(profile.protected);

        fs::remove_file(path).ok();
    }

    // --- 0600 permission warning ----------------------------------------------

    #[cfg(unix)]
    #[test]
    fn warns_on_group_readable_config_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_temp_config("default_profile = \"dev\"\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let warning = permission_warning(&path);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("chmod 600"));

        fs::remove_file(path).ok();
    }

    #[cfg(unix)]
    #[test]
    fn no_warning_for_0600_config_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_temp_config("default_profile = \"dev\"\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(permission_warning(&path).is_none());

        fs::remove_file(path).ok();
    }

    // --- redaction -------------------------------------------------------------

    #[test]
    fn resolved_profile_debug_never_leaks_secret_values() {
        let path = write_temp_config(
            "[profiles.prod.postgres]\nhost = \"pg.internal\"\npassword = \"supersecretvalue\"\n\
             [profiles.prod.mongodb]\nuri = \"mongodb://user:supersecretvalue@mongo.internal\"\n",
        );
        let cli = test_cli(Some("prod"));
        let env = empty_env();
        let profile = resolve(&cli, &env, Some(&path)).unwrap();

        let debug = format!("{profile:?}");
        assert!(!debug.contains("supersecretvalue"));

        fs::remove_file(path).ok();
    }
}
