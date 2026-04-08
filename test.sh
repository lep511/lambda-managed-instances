#!/usr/bin/env bash
set -euo pipefail

# Install GNU parallel if not available (macOS)
#brew install parallel

# Or on Linux
# sudo apt-get install parallel  # Debian/Ubuntu
# sudo yum install parallel      # RHEL/CentOS

CONCURRENCY="${1:-2}"
FUNCTION_NAME="cpu-optimized-function"

if [[ -z "${AWS_REGION:-}" ]]; then
  echo "ERROR: AWS_REGION is not set" >&2
  exit 1
fi

if ! [[ "$CONCURRENCY" =~ ^[0-9]+$ ]] || (( CONCURRENCY < 1 )); then
  echo "Usage: $0 [concurrency]  (default: 2)" >&2
  exit 1
fi

if ! command -v parallel &> /dev/null; then
  echo "ERROR: GNU parallel is not installed" >&2
  echo "  sudo yum install parallel    # RHEL/Amazon Linux" >&2
  echo "  sudo apt-get install parallel # Debian/Ubuntu" >&2
  exit 1
fi

VERSION=$(aws lambda list-versions-by-function \
  --region "$AWS_REGION" \
  --function-name "$FUNCTION_NAME" \
  --query 'Versions[-1].Version' \
  --output text)

echo "Invoking $FUNCTION_NAME (version $VERSION) x${CONCURRENCY} in parallel..."
echo "================================================================"

invoke_lambda() {
  local output="/tmp/lambda-response-${1}.json"
  aws lambda invoke \
    --region "$AWS_REGION" \
    --function-name "$FUNCTION_NAME" \
    --qualifier "$VERSION" \
    --invocation-type Event \
    --cli-binary-format raw-in-base64-out \
    --payload '{}' \
    "$output" > /dev/null 2>&1 && echo "OK" || echo "FAIL"
  rm -f "$output"
}
export -f invoke_lambda
export AWS_REGION FUNCTION_NAME VERSION

RESULTS=$(parallel -j "$CONCURRENCY" invoke_lambda ::: $(seq 1 "$CONCURRENCY"))
SUCCEEDED=$(echo "$RESULTS" | grep -c "^OK$" || true)

echo "================================================================"
echo "Done: ${SUCCEEDED}/${CONCURRENCY} queued (async)"
echo "Check CloudWatch Logs for results."
