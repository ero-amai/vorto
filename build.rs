use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=VORTO_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=VORTO_GIT_SHA");
    println!("cargo:rerun-if-env-changed=VORTO_COMMIT_RANGE");
    println!("cargo:rerun-if-env-changed=VORTO_BUILD_TIMESTAMP");
    println!("cargo:rerun-if-changed=.git/HEAD");

    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let git_sha = git_output(&["rev-parse", "--short=12", "HEAD"]);

    set_if_missing("VORTO_GIT_SHA", git_sha.as_deref());

    if env::var_os("VORTO_RELEASE_VERSION").is_none() {
        let fallback = match git_sha.as_deref() {
            Some(sha) => format!("{package_version}-dev+{sha}"),
            None => format!("{package_version}-dev"),
        };
        println!("cargo:rustc-env=VORTO_RELEASE_VERSION={fallback}");
    }

    if env::var_os("VORTO_BUILD_TIMESTAMP").is_none()
        && let Some(commit_timestamp) = git_output(&["show", "-s", "--format=%cI", "HEAD"])
    {
        println!("cargo:rustc-env=VORTO_BUILD_TIMESTAMP={commit_timestamp}");
    }
}

fn set_if_missing(key: &str, value: Option<&str>) {
    if env::var_os(key).is_none() && let Some(value) = value {
        println!("cargo:rustc-env={key}={value}");
    }
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
