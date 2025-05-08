use self_update::{self, backends::s3::*, errors::Result};
use std::env;

fn run() -> Result<()> {
    // Get AWS credentials from environment variables (recommended approach)
    // Never hardcode credentials in your code!
    //
    // NOTE: When using the aws-sdk feature, you can also use the standard AWS
    // environment variables (AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY) or
    // other AWS credential sources like ~/.aws/credentials
    //
    // With the aws-sdk feature, these will be automatically loaded from the environment
    // But we still support the explicit credential format for backward compatibility
    let aws_access_key = env::var("AWS_ACCESS_KEY").expect("AWS_ACCESS_KEY must be set");
    let aws_secret_key = env::var("AWS_SECRET_KEY").expect("AWS_SECRET_KEY must be set");

    // Format credentials as expected by the auth_token method (access_key:secret_key)
    let aws_credentials = format!("{}:{}", aws_access_key, aws_secret_key);

    println!("Listing available releases from private S3 bucket...");

    // Configure the ReleaseList to fetch releases from a private S3 bucket
    let releases = ReleaseList::configure()
        .bucket_name("my-private-releases-bucket")
        .region("us-west-2") // AWS region where your bucket is located
        .with_target("x86_64-unknown-linux-gnu") // Optional: filter for a specific target
        .auth_token(&aws_credentials) // Provide AWS credentials for authenticated access
        .build()?
        .fetch()?;

    if releases.is_empty() {
        println!("No releases found.");
        return Ok(());
    }

    println!("Found {} releases:", releases.len());

    for release in releases {
        println!("Release: {} ({})", release.name, release.version);
        println!("  Date: {}", release.date);
        println!("  Assets:");

        for asset in release.assets {
            println!("    - {}", asset.name);
        }
        println!();
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        println!("[ERROR] {}", e);
        std::process::exit(1);
    }
}
