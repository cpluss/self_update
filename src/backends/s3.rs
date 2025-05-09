/*!
Amazon S3 releases
*/
use crate::{
    errors::*,
    get_target,
    update::{Release, ReleaseAsset, ReleaseUpdate},
    version::bump_is_greater,
    DEFAULT_PROGRESS_CHARS, DEFAULT_PROGRESS_TEMPLATE,
};
use regex::Regex;
use std::cmp::Ordering;
use std::env::{self, consts::EXE_SUFFIX};
use std::path::{Path, PathBuf};

use aws_config::{BehaviorVersion, SdkConfig};
use aws_sdk_s3::{config::Region, Client as S3Client};
use tokio::runtime::Runtime;

/// Maximum number of items to retrieve from S3 API
const MAX_KEYS: i32 = 100;

/// The service end point.
///
/// Currently S3, GCS, and DigitalOcean Spaces supported.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, Default)]
pub enum EndPoint {
    #[default]
    S3,
    S3DualStack,
    GCS,
    DigitalOceanSpaces,
}

impl EndPoint {
    /// Generate the appropriate download base URL for this endpoint
    fn download_base_url(&self, bucket_name: &str, region: &str) -> String {
        match self {
            EndPoint::S3 => format!("{}.s3.{}.amazonaws.com", bucket_name, region),
            EndPoint::S3DualStack => {
                format!("{}.s3.dualstack.{}.amazonaws.com", bucket_name, region)
            }
            EndPoint::GCS => format!("{}.storage.googleapis.com", bucket_name),
            EndPoint::DigitalOceanSpaces => {
                format!("{}.{}.digitaloceanspaces.com", bucket_name, region)
            }
        }
    }

    /// Create the appropriate S3 client for this endpoint type
    fn create_client(&self, config: &SdkConfig, region: &str) -> S3Client {
        match self {
            EndPoint::S3 => S3Client::new(config),
            EndPoint::S3DualStack => {
                // Configure with dual-stack endpoint
                let s3_config = aws_sdk_s3::config::Builder::from(config)
                    .use_dual_stack(true)
                    .build();
                S3Client::from_conf(s3_config)
            }
            EndPoint::GCS => {
                // For GCS, we use a custom endpoint
                let s3_config = aws_sdk_s3::config::Builder::from(config)
                    .endpoint_url("https://storage.googleapis.com")
                    .build();
                S3Client::from_conf(s3_config)
            }
            EndPoint::DigitalOceanSpaces => {
                // For DigitalOcean Spaces, use their regional endpoint
                let endpoint_url = format!("https://{}.digitaloceanspaces.com", region);
                let s3_config = aws_sdk_s3::config::Builder::from(config)
                    .endpoint_url(endpoint_url)
                    .build();
                S3Client::from_conf(s3_config)
            }
        }
    }
}

/// `ReleaseList` Builder
#[derive(Clone, Debug)]
pub struct ReleaseListBuilder {
    end_point: EndPoint,
    bucket_name: Option<String>,
    asset_prefix: Option<String>,
    target: Option<String>,
    region: Option<String>,
    auth_token: Option<String>,
}

impl ReleaseListBuilder {
    /// Set the bucket name, used to build an S3 api url
    pub fn bucket_name(&mut self, name: &str) -> &mut Self {
        self.bucket_name = Some(name.to_owned());
        self
    }

    /// Set the optional asset name prefix, used to filter available assets with a prefix string
    pub fn asset_prefix(&mut self, prefix: &str) -> &mut Self {
        self.asset_prefix = Some(prefix.to_owned());
        self
    }

    /// Set the S3 region used in the download url
    pub fn region(&mut self, region: &str) -> &mut Self {
        self.region = Some(region.to_owned());
        self
    }

    /// Set the end point
    pub fn end_point(&mut self, end_point: EndPoint) -> &mut Self {
        self.end_point = end_point;
        self
    }

    /// Set the optional arch `target` name, used to filter available releases
    pub fn with_target(&mut self, target: &str) -> &mut Self {
        self.target = Some(target.to_owned());
        self
    }

    /// Set the authorization token or AWS credentials, used in requests to the S3 API
    ///
    /// For AWS S3 buckets that require authentication, provide credentials in the format "ACCESS_KEY:SECRET_KEY"
    ///
    /// This is to support private S3 buckets where you need AWS credentials.
    /// **Make sure not to bake the credentials into your app**; it is recommended
    /// you obtain them via another mechanism, such as environment variables
    /// or prompting the user for input
    pub fn auth_token(&mut self, auth_token: &str) -> &mut Self {
        self.auth_token = Some(auth_token.to_owned());
        self
    }

    /// Verify builder args, returning a `ReleaseList`
    pub fn build(&self) -> Result<ReleaseList> {
        let bucket_name = self
            .bucket_name
            .clone()
            .ok_or_else(|| Error::Config("`bucket_name` required".to_string()))?;

        Ok(ReleaseList {
            end_point: self.end_point,
            bucket_name,
            region: self.region.clone(),
            asset_prefix: self.asset_prefix.clone(),
            target: self.target.clone(),
            auth_token: self.auth_token.clone(),
        })
    }
}

/// `ReleaseList` provides a builder api for querying an S3 bucket,
/// returning a `Vec` of available `Release`s
#[derive(Clone, Debug)]
pub struct ReleaseList {
    end_point: EndPoint,
    bucket_name: String,
    asset_prefix: Option<String>,
    target: Option<String>,
    region: Option<String>,
    auth_token: Option<String>,
}

impl ReleaseList {
    /// Initialize a ReleaseListBuilder
    pub fn configure() -> ReleaseListBuilder {
        ReleaseListBuilder {
            end_point: EndPoint::default(),
            bucket_name: None,
            asset_prefix: None,
            target: None,
            region: None,
            auth_token: None,
        }
    }

    /// Retrieve a list of `Release`s.
    /// If specified, filter for those containing a specified `target`
    pub fn fetch(&self) -> Result<Vec<Release>> {
        let releases = fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?;

        // Filter releases by target if specified
        Ok(match &self.target {
            None => releases,
            Some(target) => releases
                .into_iter()
                .filter(|r| r.has_target_asset(target))
                .collect(),
        })
    }
}

/// `s3::Update` builder
///
/// Configure download and installation from
/// `https://<bucket_name>.s3.<region>.amazonaws.com/<asset filename>`
#[derive(Debug)]
pub struct UpdateBuilder {
    end_point: EndPoint,
    bucket_name: Option<String>,
    asset_prefix: Option<String>,
    target: Option<String>,
    region: Option<String>,
    bin_name: Option<String>,
    bin_install_path: Option<PathBuf>,
    bin_path_in_archive: Option<String>,
    show_download_progress: bool,
    show_output: bool,
    no_confirm: bool,
    current_version: Option<String>,
    target_version: Option<String>,
    progress_template: String,
    progress_chars: String,
    auth_token: Option<String>,
    #[cfg(feature = "signatures")]
    verifying_keys: Vec<[u8; zipsign_api::PUBLIC_KEY_LENGTH]>,
}

impl Default for UpdateBuilder {
    fn default() -> Self {
        Self {
            end_point: EndPoint::default(),
            bucket_name: None,
            asset_prefix: None,
            target: None,
            region: None,
            bin_name: None,
            bin_install_path: None,
            bin_path_in_archive: None,
            show_download_progress: false,
            show_output: true,
            no_confirm: false,
            current_version: None,
            target_version: None,
            progress_template: DEFAULT_PROGRESS_TEMPLATE.to_string(),
            progress_chars: DEFAULT_PROGRESS_CHARS.to_string(),
            auth_token: None,
            #[cfg(feature = "signatures")]
            verifying_keys: vec![],
        }
    }
}

/// Configure download and installation from bucket
impl UpdateBuilder {
    /// Initialize a new builder
    pub fn new() -> Self {
        Default::default()
    }

    /// Set the end point
    pub fn end_point(&mut self, end_point: EndPoint) -> &mut Self {
        self.end_point = end_point;
        self
    }

    /// Set the bucket name, used to build a s3 api url
    pub fn bucket_name(&mut self, name: &str) -> &mut Self {
        self.bucket_name = Some(name.to_owned());
        self
    }

    /// Set the optional asset name prefix, used to filter available assets with a prefix string
    pub fn asset_prefix(&mut self, prefix: &str) -> &mut Self {
        self.asset_prefix = Some(prefix.to_owned());
        self
    }

    /// Set the S3 region used in the download url
    pub fn region(&mut self, region: &str) -> &mut Self {
        self.region = Some(region.to_owned());
        self
    }

    /// Set the current app version, used to compare against the latest available version.
    /// The `cargo_crate_version!` macro can be used to pull the version from your `Cargo.toml`
    pub fn current_version(&mut self, ver: &str) -> &mut Self {
        self.current_version = Some(ver.to_owned());
        self
    }

    /// Set the target version tag to update to. This will be used to search for a release
    /// by tag name:
    /// `/repos/:owner/:repo/releases/tags/:tag`
    ///
    /// If not specified, the latest available release is used.
    pub fn target_version_tag(&mut self, ver: &str) -> &mut Self {
        self.target_version = Some(ver.to_owned());
        self
    }

    /// Set the target triple that will be downloaded, e.g. `x86_64-unknown-linux-gnu`.
    ///
    /// If unspecified, the build target of the crate will be used
    pub fn target(&mut self, target: &str) -> &mut Self {
        self.target = Some(target.to_owned());
        self
    }

    /// Set the exe's name. Also sets `bin_path_in_archive` if it hasn't already been set.
    pub fn bin_name(&mut self, name: &str) -> &mut Self {
        let raw_bin_name = format!("{}{}", name.trim_end_matches(EXE_SUFFIX), EXE_SUFFIX);
        if self.bin_path_in_archive.is_none() {
            self.bin_path_in_archive = Some(raw_bin_name.clone());
        }
        self.bin_name = Some(raw_bin_name);
        self
    }

    /// Set the installation path for the new exe, defaults to the current
    /// executable's path
    pub fn bin_install_path<A: AsRef<Path>>(&mut self, bin_install_path: A) -> &mut Self {
        self.bin_install_path = Some(PathBuf::from(bin_install_path.as_ref()));
        self
    }

    /// Set the path of the exe inside the release tarball. This is the location
    /// of the executable relative to the base of the tar'd directory and is the
    /// path that will be copied to the `bin_install_path`. If not specified, this
    /// will default to the value of `bin_name`. This only needs to be specified if
    /// the path to the binary (from the root of the tarball) is not equal to just
    /// the `bin_name`.
    ///
    /// This also supports variable paths:
    /// - `{{ bin }}` is replaced with the value of `bin_name`
    /// - `{{ target }}` is replaced with the value of `target`
    /// - `{{ version }}` is replaced with the value of `target_version` if set,
    /// otherwise the value of the latest available release version is used.
    ///
    /// # Example
    ///
    /// For a `myapp` binary with `windows` target and latest release version `1.2.3`,
    /// the tarball `myapp.tar.gz` has the contents:
    ///
    /// ```shell
    /// myapp.tar/
    ///  |------- windows-1.2.3-bin/
    ///  |         |--- myapp  # <-- executable
    /// ```
    ///
    /// The path provided should be:
    ///
    /// ```
    /// # use self_update::backends::s3::Update;
    /// # fn run() -> Result<(), Box<::std::error::Error>> {
    /// Update::configure()
    ///     .bin_path_in_archive("{{ target }}-{{ version }}-bin/{{ bin }}")
    /// #   .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn bin_path_in_archive(&mut self, bin_path: &str) -> &mut Self {
        self.bin_path_in_archive = Some(bin_path.to_owned());
        self
    }

    /// Toggle download progress bar, defaults to `off`.
    pub fn show_download_progress(&mut self, show: bool) -> &mut Self {
        self.show_download_progress = show;
        self
    }

    /// Set download progress style.
    pub fn set_progress_style(
        &mut self,
        progress_template: String,
        progress_chars: String,
    ) -> &mut Self {
        self.progress_template = progress_template;
        self.progress_chars = progress_chars;
        self
    }

    /// Toggle update output information, defaults to `true`.
    pub fn show_output(&mut self, show: bool) -> &mut Self {
        self.show_output = show;
        self
    }

    /// Toggle download confirmation. Defaults to `false`.
    pub fn no_confirm(&mut self, no_confirm: bool) -> &mut Self {
        self.no_confirm = no_confirm;
        self
    }

    /// Set the authorization token or AWS credentials, used in requests to the S3 API
    ///
    /// For AWS S3 buckets that require authentication, provide credentials in the format "ACCESS_KEY:SECRET_KEY"
    ///
    /// This is to support private S3 buckets where you need AWS credentials.
    /// **Make sure not to bake the credentials into your app**; it is recommended
    /// you obtain them via another mechanism, such as environment variables
    /// or prompting the user for input
    pub fn auth_token(&mut self, auth_token: &str) -> &mut Self {
        self.auth_token = Some(auth_token.to_owned());
        self
    }

    /// Specify a slice of ed25519ph verifying keys to validate a download's authenticy
    ///
    /// If the feature is activated AND at least one key was provided, a download is verifying.
    /// At least one key has to match.
    #[cfg(feature = "signatures")]
    pub fn verifying_keys(
        &mut self,
        keys: impl Into<Vec<[u8; zipsign_api::PUBLIC_KEY_LENGTH]>>,
    ) -> &mut Self {
        self.verifying_keys = keys.into();
        self
    }

    /// Confirm config and create a ready-to-use `Update`
    ///
    /// * Errors:
    ///     * Config - Invalid `Update` configuration
    pub fn build(&self) -> Result<Box<dyn ReleaseUpdate>> {
        // Use functional combinators for optional values
        let bin_install_path =
            self.bin_install_path
                .clone()
                .unwrap_or_else(|| match env::current_exe() {
                    Ok(path) => path,
                    Err(e) => panic!("Failed to get current executable path: {}", e),
                });

        let bucket_name = self
            .bucket_name
            .clone()
            .ok_or_else(|| Error::Config("`bucket_name` required".to_string()))?;

        let target = self
            .target
            .clone()
            .unwrap_or_else(|| get_target().to_owned());

        let bin_name = self
            .bin_name
            .clone()
            .ok_or_else(|| Error::Config("`bin_name` required".to_string()))?;

        let bin_path_in_archive = self
            .bin_path_in_archive
            .clone()
            .ok_or_else(|| Error::Config("`bin_path_in_archive` required".to_string()))?;

        let current_version = self
            .current_version
            .clone()
            .ok_or_else(|| Error::Config("`current_version` required".to_string()))?;

        Ok(Box::new(Update {
            end_point: self.end_point,
            bucket_name,
            region: self.region.clone(),
            asset_prefix: self.asset_prefix.clone(),
            target,
            bin_name,
            bin_install_path,
            bin_path_in_archive,
            current_version,
            target_version: self.target_version.clone(),
            show_download_progress: self.show_download_progress,
            progress_template: self.progress_template.clone(),
            progress_chars: self.progress_chars.clone(),
            show_output: self.show_output,
            no_confirm: self.no_confirm,
            auth_token: self.auth_token.clone(),
            #[cfg(feature = "signatures")]
            verifying_keys: self.verifying_keys.clone(),
        }))
    }
}

/// Updates to a specified or latest release distributed via S3
#[derive(Debug)]
pub struct Update {
    end_point: EndPoint,
    bucket_name: String,
    asset_prefix: Option<String>,
    target: String,
    region: Option<String>,
    current_version: String,
    target_version: Option<String>,
    bin_name: String,
    bin_install_path: PathBuf,
    bin_path_in_archive: String,
    show_download_progress: bool,
    show_output: bool,
    no_confirm: bool,
    progress_template: String,
    progress_chars: String,
    auth_token: Option<String>,
    #[cfg(feature = "signatures")]
    verifying_keys: Vec<[u8; zipsign_api::PUBLIC_KEY_LENGTH]>,
}

impl Update {
    /// Initialize a new `Update` builder
    pub fn configure() -> UpdateBuilder {
        UpdateBuilder::new()
    }
}

impl ReleaseUpdate for Update {
    fn get_latest_release(&self) -> Result<Release> {
        fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?
        .into_iter()
        .max_by(|x, y| compare_versions(&x.version, &y.version))
        .ok_or_else(|| Error::Release("No release was found".to_string()))
    }

    fn get_latest_releases(&self, current_version: &str) -> Result<Vec<Release>> {
        let releases = fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?;

        // Filter releases newer than current version
        let mut releases = releases
            .into_iter()
            .filter(|r| bump_is_greater(current_version, &r.version).unwrap_or(false))
            .collect::<Vec<_>>();

        // Sort by version (descending)
        releases.sort_by(|x, y| compare_versions(&y.version, &x.version));

        Ok(releases)
    }

    fn get_release_version(&self, ver: &str) -> Result<Release> {
        fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?
        .into_iter()
        .find(|x| x.version == ver)
        .ok_or_else(|| Error::Release(format!("No release with version '{}' was found", ver)))
    }

    fn current_version(&self) -> String {
        self.current_version.clone()
    }

    fn target(&self) -> String {
        self.target.clone()
    }

    fn target_version(&self) -> Option<String> {
        self.target_version.clone()
    }

    fn bin_name(&self) -> String {
        self.bin_name.clone()
    }

    fn bin_install_path(&self) -> PathBuf {
        self.bin_install_path.clone()
    }

    fn bin_path_in_archive(&self) -> String {
        self.bin_path_in_archive.clone()
    }

    fn show_download_progress(&self) -> bool {
        self.show_download_progress
    }

    fn show_output(&self) -> bool {
        self.show_output
    }

    fn no_confirm(&self) -> bool {
        self.no_confirm
    }

    fn progress_template(&self) -> String {
        self.progress_template.clone()
    }

    fn progress_chars(&self) -> String {
        self.progress_chars.clone()
    }

    fn auth_token(&self) -> Option<String> {
        self.auth_token.clone()
    }

    #[cfg(feature = "signatures")]
    fn verifying_keys(&self) -> &[[u8; zipsign_api::PUBLIC_KEY_LENGTH]] {
        &self.verifying_keys
    }
}

/// Compare two version strings for ordering
///
/// Returns Ordering::Greater if v1 > v2, Ordering::Less if v1 < v2,
/// and Ordering::Equal if they're equal or comparison fails
fn compare_versions(v1: &str, v2: &str) -> Ordering {
    match bump_is_greater(v2, v1) {
        Ok(true) => Ordering::Greater,
        Ok(false) => Ordering::Less,
        Err(_) => Ordering::Less, // Error case - consider it less
    }
}

/// Obtain list of releases from AWS S3 API, from bucket and region specified,
/// filtering assets which don't match the prefix string if provided.
///
/// This will strip the prefix from provided file names, allowing use with subdirectories
fn fetch_releases_from_s3(
    end_point: EndPoint,
    bucket_name: &str,
    region: &Option<String>,
    asset_prefix: &Option<String>,
    auth_token: &Option<String>,
) -> Result<Vec<Release>> {
    // Extract region or return error if not provided
    let region_str = region
        .as_ref()
        .ok_or_else(|| Error::Config("`region` required".to_string()))?;

    fetch_releases_with_aws_sdk(end_point, bucket_name, region_str, asset_prefix, auth_token)
}

/// Create an AWS SDK config with the given credentials
async fn create_aws_config(region: &str, auth_token: &Option<String>) -> SdkConfig {
    // Start with default config that loads credentials from all standard locations
    let mut config_builder =
        aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.to_string()));

    // Apply explicit credentials if provided in auth_token (format: "ACCESS_KEY:SECRET_KEY")
    if let Some(auth) = auth_token {
        if let Some((access_key, secret_key)) = auth.split_once(':') {
            debug!("Using provided AWS credentials");

            // Import necessary types
            use aws_sdk_s3::config::Credentials;

            // Create credentials provider with the provided credentials
            let credentials = Credentials::new(
                access_key,
                secret_key,
                None, // session token
                None, // expiry time
                "self_update-provided",
            );

            config_builder = config_builder.credentials_provider(credentials);
        }
    }

    debug!("Loading AWS configuration");
    config_builder.load().await
}

/// Fetch releases from S3 using the AWS SDK
fn fetch_releases_with_aws_sdk(
    end_point: EndPoint,
    bucket_name: &str,
    region: &str,
    asset_prefix: &Option<String>,
    auth_token: &Option<String>,
) -> Result<Vec<Release>> {
    // Create a tokio runtime for async AWS SDK operations
    let runtime = Runtime::new()
        .map_err(|e| Error::Network(format!("Failed to create async runtime: {}", e)))?;

    let config = runtime.block_on(create_aws_config(region, auth_token));
    let s3_client = end_point.create_client(&config, region);
    let download_base_url = end_point.download_base_url(bucket_name, region);

    // Build request parameters for debugging
    let prefix_str = asset_prefix.as_ref().map_or("None", |s| s.as_str());
    debug!(
        "Making S3 list_objects_v2 request to bucket '{}' in region '{}' with prefix '{}'",
        bucket_name, region, prefix_str
    );

    let mut releases = Vec::new();
    let mut continuation_token = None;

    // Execute the request in the runtime with pagination support
    loop {
        debug!(
            "Making S3 list_objects_v2 request to bucket '{}' (page: {})",
            bucket_name,
            if continuation_token.is_some() {
                "continuation"
            } else {
                "first"
            }
        );

        let list_result = runtime.block_on(async {
            let mut request = s3_client
                .list_objects_v2()
                .bucket(bucket_name)
                .max_keys(MAX_KEYS);

            // Only set prefix if specified
            if let Some(prefix) = asset_prefix {
                request = request.prefix(prefix);
            }

            // Add continuation token if we're not on the first page
            if let Some(token) = &continuation_token {
                request = request.continuation_token(token);
            }

            request.send().await
        });

        // Handle potential error
        let list_output = list_result
            .map_err(|err| Error::Network(format!("Failed to list S3 objects: {}", err)))?;

        // Process the current page of results
        let contents = list_output.contents();

        for obj in contents {
            if let Some(key) = obj.key() {
                let last_modified = obj
                    .last_modified()
                    .map(|dt| dt.to_string())
                    .unwrap_or_default();

                if let Err(e) =
                    process_s3_object(key, &last_modified, &download_base_url, &mut releases)
                {
                    debug!("Error processing S3 object {}: {}", key, e);
                }
            }
        }

        // Check if there are more pages
        continuation_token = list_output.next_continuation_token().map(|s| s.to_string());

        // If there's no continuation token, we've processed all pages
        if continuation_token.is_none() {
            break;
        }

        debug!("Fetching next page of S3 bucket contents");
    }

    Ok(releases)
}

/// Process an S3 object and add it to the releases list if it represents a valid release
fn process_s3_object(
    key: &str,
    last_modified: &str,
    download_base_url: &str,
    releases: &mut Vec<Release>,
) -> Result<()> {
    // Extract filename from key
    let p = PathBuf::from(key);
    let exe_name = match p.file_name().and_then(|v| v.to_str()) {
        Some(v) => v,
        _ => key,
    };

    // Extract version directly from path components
    // Expected format: [directory/][semver/]<asset name>-<platform/target>.<extension>
    let path_components: Vec<&str> = key.split('/').collect();

    debug!("Analyzing path components for key: {}", key);
    if path_components.len() >= 2 {
        // Check if second-to-last component looks like a semver
        let potential_version = path_components[path_components.len() - 2];
        debug!("Checking if '{}' is a version directory", potential_version);

        // Simple semver check for x.y.z with optional pre-release and optional 'v' prefix
        let semver_regex = Regex::new(r"^v?\d+\.\d+\.\d+(?:-[a-z0-9.]+)*$")
            .map_err(|err| Error::Release(format!("Failed to create semver regex: {}", err)))?;

        if semver_regex.is_match(potential_version) {
            // We found a version directory containing assets
            let asset_filename = path_components.last().unwrap();

            // Extract base asset name from filename
            let file_parts: Vec<&str> = asset_filename
                .split('.')
                .next()
                .unwrap()
                .split('-')
                .collect();
            let asset_name = extract_asset_name(&file_parts);

            let release = Release {
                name: asset_name,
                version: potential_version.trim_start_matches('v').to_string(),
                date: last_modified.to_string(),
                assets: vec![ReleaseAsset {
                    name: exe_name.to_string(),
                    download_url: format!(
                        "s3://{}/{}",
                        download_base_url.trim_end_matches('/'),
                        key
                    ),
                }],
                body: None,
            };

            debug!(
                "Matched release from directory structure {}: {:?}",
                key, &release
            );
            add_to_releases_list(releases, release);
            return Ok(());
        }
    }

    Ok(())
}

/// Extract the asset name from file parts
fn extract_asset_name(file_parts: &[&str]) -> String {
    // Find where the target part starts by looking for architecture prefixes
    let mut target_start_idx = 0;
    for (idx, part) in file_parts.iter().enumerate() {
        if ["x86_64", "aarch64", "i686", "armv7"].contains(part) {
            target_start_idx = idx;
            break;
        }
    }

    // If we found a target pattern, join all parts before it as the asset name
    if target_start_idx > 0 {
        file_parts[0..target_start_idx].join("-")
    } else {
        // Fallback to first part if we can't identify by architecture
        file_parts[0].to_string()
    }
}

// Add a release to the list if it doesn't exist yet, or merge its asset/s
// details into the release item already existing in the list
fn add_to_releases_list(releases: &mut Vec<Release>, mut rel: Release) {
    if !rel.version.is_empty() && !rel.name.is_empty() {
        match releases
            .iter_mut()
            .find(|curr| curr.name == rel.name && curr.version == rel.version)
        {
            Some(existing) => {
                // Merge assets into the existing release
                existing.assets.append(&mut rel.assets);
            }
            None => releases.push(rel),
        }
    }
}
