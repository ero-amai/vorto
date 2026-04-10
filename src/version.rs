const APP_NAME: &str = env!("CARGO_PKG_NAME");
const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASE_VERSION: &str = env!("VORTO_RELEASE_VERSION");
const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");

pub fn print() {
    println!("{APP_NAME} {RELEASE_VERSION}");
    println!("Package version: {PACKAGE_VERSION}");
    println!("Repository: {REPOSITORY_URL}");

    if let Some(commit) = git_sha() {
        println!("Commit: {commit}");
    }

    if let Some(commit_range) = commit_range() {
        println!("Commit range: {commit_range}");
    }

    if let Some(build_timestamp) = build_timestamp() {
        println!("Built at: {build_timestamp}");
    }
}

fn git_sha() -> Option<&'static str> {
    option_env!("VORTO_GIT_SHA").filter(|value| !value.is_empty())
}

fn commit_range() -> Option<&'static str> {
    option_env!("VORTO_COMMIT_RANGE").filter(|value| !value.is_empty())
}

fn build_timestamp() -> Option<&'static str> {
    option_env!("VORTO_BUILD_TIMESTAMP").filter(|value| !value.is_empty())
}
