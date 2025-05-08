use self_update::{self, backends::s3::*, errors::Result};
use std::env;

const APP_VERSION: &str = "0.1.0";

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

    println!("Current version is {}", APP_VERSION);
    println!("Checking for updates from private S3 bucket...");
    
    let status = Update::configure()
        .bucket_name("my-private-releases-bucket")
        .region("us-west-2")              // AWS region where your bucket is located
        .bin_name("my_app")               // The name of your application's binary
        .current_version(APP_VERSION)     // Current app version, used to compare against latest available
        .target("x86_64-unknown-linux-gnu") // Target triple, used to find the right asset
        .auth_token(&aws_credentials)     // Provide AWS credentials for authenticated access
        .build()?
        .update()?;

    println!("Update status: `{}`!", status.version());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        println!("[ERROR] {}", e);
        std::process::exit(1);
    }
}