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

#[cfg(feature = "aws-sdk")]
use {
    aws_config::{BehaviorVersion, SdkConfig},
    aws_sdk_s3::{
        config::{Credentials, Region},
        Client as S3Client,
    },
    tokio::runtime::Runtime,
};

#[cfg(not(feature = "aws-sdk"))]
use {quick_xml::events::Event, quick_xml::Reader};

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
        Ok(ReleaseList {
            end_point: self.end_point,
            bucket_name: if let Some(ref name) = self.bucket_name {
                name.to_owned()
            } else {
                bail!(Error::Config, "`bucket_name` required")
            },
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
        let releases = match self.target {
            None => releases,
            Some(ref target) => releases
                .into_iter()
                .filter(|r| r.has_target_asset(target))
                .collect::<Vec<_>>(),
        };
        Ok(releases)
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
            self.bin_path_in_archive = Some(raw_bin_name.to_owned());
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
        let bin_install_path = if let Some(v) = &self.bin_install_path {
            v.clone()
        } else {
            env::current_exe()?
        };

        Ok(Box::new(Update {
            end_point: self.end_point,
            bucket_name: if let Some(ref name) = self.bucket_name {
                name.to_owned()
            } else {
                bail!(Error::Config, "`bucket_name` required")
            },
            region: self.region.clone(),
            asset_prefix: self.asset_prefix.clone(),
            target: self
                .target
                .as_ref()
                .map(|t| t.to_owned())
                .unwrap_or_else(|| get_target().to_owned()),
            bin_name: if let Some(ref name) = self.bin_name {
                name.to_owned()
            } else {
                bail!(Error::Config, "`bin_name` required")
            },
            bin_install_path,
            bin_path_in_archive: if let Some(ref bin_path) = self.bin_path_in_archive {
                bin_path.to_owned()
            } else {
                bail!(Error::Config, "`bin_path_in_archive` required")
            },
            current_version: if let Some(ref ver) = self.current_version {
                ver.to_owned()
            } else {
                bail!(Error::Config, "`current_version` required")
            },
            target_version: self.target_version.as_ref().map(|v| v.to_owned()),
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
        let releases = fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?;
        let rel = releases
            .iter()
            .max_by(|x, y| match bump_is_greater(&y.version, &x.version) {
                Ok(is_greater) => {
                    if is_greater {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                Err(_) => {
                    // Ignoring release due to an unexpected failure in parsing its version string
                    Ordering::Less
                }
            });

        match rel {
            Some(r) => Ok(r.clone()),
            None => bail!(Error::Release, "No release was found"),
        }
    }

    fn get_latest_releases(&self, current_version: &str) -> Result<Vec<Release>> {
        let releases = fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?;

        let mut releases = releases
            .iter()
            .filter(|r| bump_is_greater(current_version, &r.version).unwrap_or(false))
            .cloned()
            .collect::<Vec<_>>();

        releases.sort_by(|x, y| match bump_is_greater(&y.version, &x.version) {
            Ok(is_greater) => {
                if is_greater {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            Err(_) => {
                // Ignoring release due to an unexpected failure in parsing its version string
                Ordering::Less
            }
        });

        Ok(releases)
    }

    fn get_release_version(&self, ver: &str) -> Result<Release> {
        let releases = fetch_releases_from_s3(
            self.end_point,
            &self.bucket_name,
            &self.region,
            &self.asset_prefix,
            &self.auth_token,
        )?;
        let rel = releases.iter().find(|x| x.version == ver);
        match rel {
            Some(r) => Ok(r.clone()),
            None => bail!(
                Error::Release,
                "No release with version '{}' was found",
                ver
            ),
        }
    }

    fn current_version(&self) -> String {
        self.current_version.to_owned()
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
        self.progress_template.to_owned()
    }

    fn progress_chars(&self) -> String {
        self.progress_chars.to_owned()
    }

    fn auth_token(&self) -> Option<String> {
        self.auth_token.clone()
    }

    #[cfg(feature = "signatures")]
    fn verifying_keys(&self) -> &[[u8; zipsign_api::PUBLIC_KEY_LENGTH]] {
        &self.verifying_keys
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
    let region_str = region
        .as_ref()
        .ok_or_else(|| Error::Config("`region` required".to_string()))?;

    #[cfg(feature = "aws-sdk")]
    return fetch_releases_with_aws_sdk(
        end_point,
        bucket_name,
        region_str,
        asset_prefix,
        auth_token,
    );

    #[cfg(not(feature = "aws-sdk"))]
    return fetch_releases_with_reqwest(
        end_point,
        bucket_name,
        region_str,
        asset_prefix,
        auth_token,
    );
}

#[cfg(feature = "aws-sdk")]
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

    let config = runtime.block_on(async {
        // Build AWS SDK configuration with the following credential sources (in order):
        // 1. Explicit credentials from auth_token if provided
        // 2. Environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)
        // 3. AWS credential files (~/.aws/credentials)
        // 4. IAM roles for Amazon EC2 / container credentials
        let region_provider = Region::new(region.to_string());

        // Start with default config that loads credentials from all standard locations
        let mut config_builder =
            aws_config::defaults(BehaviorVersion::latest()).region(region_provider);

        // Configure explicit credentials if provided via auth_token
        if let Some(token) = auth_token {
            if token.contains(':') {
                let mut parts = token.splitn(2, ':');
                if let (Some(access_key), Some(secret_key)) = (parts.next(), parts.next()) {
                    let credentials = Credentials::new(
                        access_key,
                        secret_key,
                        None, // session token
                        None, // expiry time
                        "self-update",
                    );
                    config_builder = config_builder.credentials_provider(credentials);
                    debug!("Using AWS credentials from auth_token parameter");
                }
            }
        }

        debug!("Loading AWS configuration from environment and credential files");
        config_builder.load().await
    });

    // Create S3 client
    let s3_client = get_s3_client(end_point, &config, region)?;

    // We'll directly use the list_objects_v2 method from the client below
    // No need to create a builder separately

    // Build request parameters for debugging
    let prefix_str = asset_prefix.as_ref().map_or("None", |s| s.as_str());
    debug!(
        "Making S3 list_objects_v2 request to bucket '{}' in region '{}' with prefix '{}'",
        bucket_name, region, prefix_str
    );

    // Get the endpoint URL for constructing download URLs
    let download_base_url = get_download_base_url(end_point, bucket_name, region)?;
    let mut releases = Vec::new();
    let mut continuation_token = None;

    // Create regex for parsing filenames to extract version information
    let regex = Regex::new(r"(?i)(?P<prefix>.*/)*(?P<n>.+)-[v]{0,1}(?P<version>\d+\.\d+\.\d+)-.+")
        .map_err(|err| {
            Error::Release(format!(
                "Failed constructing regex to parse S3 filenames: {}",
                err
            ))
        })?;

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
        let list_output = match list_result {
            Ok(output) => output,
            Err(err) => {
                bail!(Error::Network, "Failed to list S3 objects: {}", err);
            }
        };

        // Process the current page of results
        let contents = list_output.contents();

        // Process objects in this page
        for obj in contents {
            let key = match obj.key() {
                Some(k) => k,
                None => continue, // Skip objects without keys
            };

            let last_modified = obj
                .last_modified()
                .map(|dt| dt.to_string())
                .unwrap_or_default();

            process_s3_object(
                key,
                &last_modified,
                &download_base_url,
                &regex,
                &mut releases,
            )?;
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

#[cfg(feature = "aws-sdk")]
fn process_s3_object(
    key: &str,
    last_modified: &str,
    download_base_url: &str,
    regex: &Regex,
    releases: &mut Vec<Release>,
) -> Result<()> {
    // Extract filename from key
    let p = PathBuf::from(key);
    let exe_name = match p.file_name().map(|v| v.to_str()) {
        Some(Some(v)) => v,
        _ => key,
    };

    // Use regex to extract version information
    if let Some(captures) = regex.captures(key) {
        let mut release = Release::default();
        release.name = captures["name"].to_string();
        release.version = captures["version"].trim_start_matches('v').to_string();
        release.date = last_modified.to_string();
        release.assets = vec![ReleaseAsset {
            name: exe_name.to_string(),
            download_url: format!("{}{}", download_base_url, key),
        }];

        debug!("Matched release from key {}: {:?}", key, &release);
        add_to_releases_list(releases, release);
    } else {
        debug!("Regex mismatch for key: {}", key);
    }

    Ok(())
}

#[cfg(feature = "aws-sdk")]
fn get_s3_client(end_point: EndPoint, config: &SdkConfig, region: &str) -> Result<S3Client> {
    Ok(match end_point {
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
    })
}

// Function to get the download base URL for assets based on endpoint type
fn get_download_base_url(end_point: EndPoint, bucket_name: &str, region: &str) -> Result<String> {
    let download_base_url = match end_point {
        EndPoint::S3 => format!("https://{}.s3.{}.amazonaws.com/", bucket_name, region),
        EndPoint::S3DualStack => format!(
            "https://{}.s3.dualstack.{}.amazonaws.com/",
            bucket_name, region
        ),
        EndPoint::DigitalOceanSpaces => {
            format!("https://{}.{}.digitaloceanspaces.com/", bucket_name, region)
        }
        EndPoint::GCS => format!("https://storage.googleapis.com/{}/", bucket_name),
    };

    Ok(download_base_url)
}

#[cfg(not(feature = "aws-sdk"))]
fn fetch_releases_with_reqwest(
    end_point: EndPoint,
    bucket_name: &str,
    region: &str,
    asset_prefix: &Option<String>,
    auth_token: &Option<String>,
) -> Result<Vec<Release>> {
    let download_base_url = get_download_base_url(end_point, bucket_name, region)?;
    let mut releases = Vec::new();
    let mut continuation_token = None;
    let regex =
        Regex::new(r"(?i)(?P<prefix>.*/)*(?P<name>.+)-[v]{0,1}(?P<version>\d+\.\d+\.\d+)-.+")
            .map_err(|err| {
                Error::Release(format!(
                    "Failed constructing regex to parse S3 filenames: {}",
                    err
                ))
            })?;

    let client = reqwest::blocking::ClientBuilder::new()
        .use_rustls_tls()
        .http2_adaptive_window(true)
        .build()?;

    // Implement pagination for the standard S3 API
    loop {
        // Create the API URL with appropriate parameters
        let mut params = vec![format!("list-type=2"), format!("max-keys={}", MAX_KEYS)];

        // Add prefix if specified
        if let Some(prefix) = asset_prefix {
            params.push(format!("prefix={}", prefix));
        }

        // Add continuation token if we have one
        if let Some(token) = &continuation_token {
            params.push(format!("continuation-token={}", token));
        }

        // Build the complete URL
        let api_url = match end_point {
            EndPoint::S3 | EndPoint::S3DualStack | EndPoint::DigitalOceanSpaces => {
                format!("{}?{}", download_base_url, params.join("&"))
            }
            EndPoint::GCS => format!(
                "{}?{}",
                download_base_url,
                params.join("&").replace("list-type=2&", "")
            ),
        };

        debug!(
            "Using API URL: {:?} (page: {})",
            api_url,
            if continuation_token.is_some() {
                "continuation"
            } else {
                "first"
            }
        );

        // Build the request with authentication if provided
        let mut request_builder = client.get(&api_url);

        // Apply authentication if provided
        if let Some(token) = auth_token {
            // Check if the token appears to be formatted as AWS credentials
            if token.contains(':') {
                let mut parts = token.splitn(2, ':');
                if let (Some(access_key), Some(secret_key)) = (parts.next(), parts.next()) {
                    let auth_header = format!("AWS {}", access_key);
                    request_builder = request_builder
                        .header(reqwest::header::AUTHORIZATION, &auth_header)
                        .header("X-Amz-Secret-Key", secret_key);

                    debug!("Using AWS authentication with provided credentials");
                } else {
                    debug!("Malformed AWS credentials format, expected 'access_key:secret_key'");
                }
            } else {
                // If it's not in the expected format, use it as a bearer token
                request_builder = request_builder.bearer_auth(token);
                debug!("Using bearer token authentication");
            }
        }

        // Send the request
        let resp = request_builder.send()?;

        if !resp.status().is_success() {
            bail!(
                Error::Network,
                "S3 API request failed with status: {:?} - for: {:?}",
                resp.status(),
                api_url
            )
        }

        let body = resp.text()?;
        let mut reader = Reader::from_str(&body);
        reader.config_mut().trim_text(true);

        // Parse the response to extract the releases
        enum Tag {
            Contents,
            Key,
            LastModified,
            NextContinuationToken,
            IsTruncated,
            Other,
        }

        let mut current_tag = Tag::Other;
        let mut current_release: Option<Release> = None;
        let mut new_continuation_token = None;
        let mut is_truncated = false;

        // Process XML response
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => match e.name().into_inner() {
                    b"Contents" => {
                        current_tag = Tag::Contents;
                        if let Some(release) = current_release {
                            add_to_releases_list(&mut releases, release);
                        }
                        current_release = None;
                    }
                    b"Key" => current_tag = Tag::Key,
                    b"LastModified" => current_tag = Tag::LastModified,
                    b"NextContinuationToken" => current_tag = Tag::NextContinuationToken,
                    b"IsTruncated" => current_tag = Tag::IsTruncated,
                    _ => current_tag = Tag::Other,
                },
                Ok(Event::Text(e)) => {
                    // If we cannot decode a tag text we just ignore it
                    if let Ok(txt) = e.unescape().map(|r| r.into_owned()) {
                        match current_tag {
                            Tag::Key => {
                                let p = PathBuf::from(&txt);
                                let exe_name = match p.file_name().map(|v| v.to_str()) {
                                    Some(Some(v)) => v,
                                    _ => &txt,
                                };

                                if let Some(captures) = regex.captures(&txt) {
                                    let release = current_release.get_or_insert(Release::default());
                                    release.name = captures["name"].to_string();
                                    release.version =
                                        captures["version"].trim_start_matches('v').to_string();
                                    release.assets = vec![ReleaseAsset {
                                        name: exe_name.to_string(),
                                        download_url: format!("{}{}", download_base_url, txt),
                                    }];
                                    debug!("Matched release: {:?}", release);
                                } else {
                                    debug!("Regex mismatch: {:?}", &txt);
                                }
                            }
                            Tag::LastModified => {
                                let release = current_release.get_or_insert(Release::default());
                                release.date = txt;
                            }
                            Tag::NextContinuationToken => {
                                new_continuation_token = Some(txt);
                            }
                            Tag::IsTruncated => {
                                is_truncated = txt.to_lowercase() == "true";
                            }
                            _ => (),
                        }
                    }
                }
                Ok(Event::Eof) => {
                    if let Some(release) = current_release {
                        add_to_releases_list(&mut releases, release);
                    }
                    break; // exits the loop when reaching end of file
                }
                Err(e) => bail!(
                    Error::Release,
                    "Failed when parsing S3 XML response at position {}: {:?}",
                    reader.buffer_position(),
                    e
                ),
                _ => (), // There are several other `Event`s we ignore here
            }

            buf.clear();
        }

        // If the response is not truncated or there's no continuation token, we're done
        if !is_truncated || new_continuation_token.is_none() {
            break;
        }

        // Update continuation token and fetch next page
        continuation_token = new_continuation_token;
        debug!("Fetching next page of S3 bucket contents");
    }

    Ok(releases)
}

// Add a release to the list if it's doesn't exist yet, or merge its asset/s
// details into the release item already existing in the list
fn add_to_releases_list(releases: &mut Vec<Release>, mut rel: Release) {
    if !rel.version.is_empty() && !rel.name.is_empty() {
        match releases
            .iter()
            .position(|curr| curr.name == rel.name && curr.version == rel.version)
        {
            Some(index) => {
                rel.assets.append(&mut releases[index].assets);
                releases.push(rel);
                releases.swap_remove(index);
            }
            None => releases.push(rel),
        }
    }
}
