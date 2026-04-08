#!/usr/bin/env bash
set -euo pipefail

# Verify required environment variables
for var in AWS_REGION LAMBDA_ROLE_ARN CAPACITY_PROVIDER_ARN; do
  if [[ -z "${!var:-}" ]]; then
    echo "ERROR: $var is not set" >&2
    exit 1
  fi
done

# Resolve script directory to absolute path
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Build the release binary (cross-compiled for aarch64 via .cargo/config.toml)
echo "Building Rust Lambda function..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

# Package the bootstrap binary into a zip
TARGET_DIR="$SCRIPT_DIR/target/aarch64-unknown-linux-gnu/release"
ZIP_FILE="$SCRIPT_DIR/lambda-function.zip"

cp "$TARGET_DIR/bootstrap" /tmp/bootstrap
chmod 755 /tmp/bootstrap
(cd /tmp && zip -j "$ZIP_FILE" bootstrap)
rm /tmp/bootstrap

echo "Created $ZIP_FILE"

# Deploy or update function
FUNCTION_NAME="cpu-optimized-function"

if aws lambda get-function --region "$AWS_REGION" --function-name "$FUNCTION_NAME" > /dev/null 2>&1; then
  echo "Function $FUNCTION_NAME exists. Updating code..."
  aws lambda update-function-code \
    --region "$AWS_REGION" \
    --function-name "$FUNCTION_NAME" \
    --zip-file "fileb://$ZIP_FILE"

  echo "Waiting for update to complete..."
  aws lambda wait function-updated-v2 \
    --region "$AWS_REGION" \
    --function-name "$FUNCTION_NAME"
else
  echo "Creating function $FUNCTION_NAME..."
  aws lambda create-function \
    --region "$AWS_REGION" \
    --function-name "$FUNCTION_NAME" \
    --runtime provided.al2023 \
    --role "$LAMBDA_ROLE_ARN" \
    --handler bootstrap \
    --zip-file "fileb://$ZIP_FILE" \
    --timeout 180 \
    --memory-size 4096 \
    --architectures arm64 \
    --tracing-config Mode=Active \
    --environment "Variables={RUST_LOG=info}" \
    --capacity-provider-config "{\"LambdaManagedInstancesCapacityProviderConfig\":{\"CapacityProviderArn\":\"$CAPACITY_PROVIDER_ARN\"}}"

  echo ""
  echo "----------------------------------------------------------------"
  echo "Waiting 90 seconds for function to be ready..."
  sleep 90
fi

# Publish new version
echo ""
echo "----------------------------------------------------------------"
echo "Publishing function version..."
aws lambda publish-version \
  --region "$AWS_REGION" \
  --function-name "$FUNCTION_NAME"

