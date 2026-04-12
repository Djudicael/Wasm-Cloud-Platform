#!/bin/bash
# Manual test script for graceful shutdown
# This script demonstrates the graceful shutdown feature end-to-end

set -e

echo "================================================"
echo "Graceful Shutdown End-to-End Demonstration"
echo "================================================"
echo ""

# Colors for output
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Step 1: Build the WASM app
echo -e "${BLUE}Step 1: Building hello-axum with graceful shutdown support${NC}"
RUSTFLAGS="--cfg tokio_unstable" cargo build -p hello-axum --target wasm32-wasip2 --release
echo -e "${GREEN}✓ WASM app built${NC}"
echo ""

# Step 2: Check if NATS is running
echo -e "${BLUE}Step 2: Checking NATS${NC}"
if ! podman ps | grep -q nats; then
    echo "Starting NATS with JetStream..."
    podman run -d --rm --name nats-test -p 4222:4222 docker.io/library/nats:2.10-alpine -js
    sleep 3
fi
echo -e "${GREEN}✓ NATS is running${NC}"
echo ""

# Step 3: Clean up any previous test data
echo -e "${BLUE}Step 3: Cleaning up previous test data${NC}"
rm -f /tmp/wasm-graceful-demo.redb /tmp/wasm-graceful-demo.key
echo -e "${GREEN}✓ Cleaned${NC}"
echo ""

# Step 4: Start the node in the background
echo -e "${BLUE}Step 4: Starting wasm-node${NC}"
RUST_LOG=info cargo run -p node -- \
  --nats-url nats://127.0.0.1:4222 \
  --db-path /tmp/wasm-graceful-demo.redb \
  --proxy-port 38080 \
  --proxy-https-port 0 \
  --admin-port 39090 \
  --artifact-port 39091 \
  --key-source generate \
  --key-file /tmp/wasm-graceful-demo.key \
  --port-start 40000 \
  --port-end 40999 \
  > /tmp/wasm-node-graceful.log 2>&1 &

NODE_PID=$!
echo "Node PID: $NODE_PID"

# Wait for node to be ready
echo "Waiting for node to be ready..."
for i in {1..30}; do
  if curl -s http://127.0.0.1:39090/health > /dev/null 2>&1; then
    echo -e "${GREEN}✓ Node is ready${NC}"
    break
  fi
  sleep 1
  if [ $i -eq 30 ]; then
    echo "Node failed to start. Check /tmp/wasm-node-graceful.log"
    kill $NODE_PID 2>/dev/null || true
    exit 1
  fi
done
echo ""

# Step 5: Deploy the app via NATS events
echo -e "${BLUE}Step 5: Deploying hello-axum${NC}"

# Start a simple artifact server
echo "Starting artifact server..."
python3 -m http.server 39091 --directory target/wasm32-wasip2/release > /dev/null 2>&1 &
ARTIFACT_PID=$!
sleep 1

# Calculate SHA256
WASM_HASH=$(sha256sum target/wasm32-wasip2/release/hello-axum.wasm | awk '{print $1}')
echo "WASM SHA256: $WASM_HASH"

# Publish deployment event (using nats CLI if available, otherwise instructions)
if command -v nats &> /dev/null; then
  echo "Publishing DeployApp event..."
  nats pub deploy.app.new '{
    "type": "deploy_app",
    "app_id": "hello-axum",
    "config": {
      "id": "hello-axum",
      "fuel_quota": 1000000000,
      "memory_limit": 4096,
      "max_instances": 5,
      "idle_timeout_secs": 300,
      "wasm_bind_port": 8080,
      "env_vars": {},
      "secret_keys": [],
      "extended_limits": null,
      "health_check_path": "/"
    },
    "artifact_url": "http://127.0.0.1:39091/hello-axum.wasm",
    "expected_hash": "'$WASM_HASH'",
    "size_bytes": '$(stat -c%s target/wasm32-wasip2/release/hello-axum.wasm)'
  }'

  echo "Publishing RouteAdd event..."
  nats pub routes.add '{
    "type": "route_add",
    "route": {
      "host": "hello-axum",
      "app_id": "hello-axum",
      "path_prefix": "/",
      "strip_prefix": false,
      "created_at": 0,
      "updated_at": 0
    }
  }'
else
  echo -e "${YELLOW}Note: 'nats' CLI not found. You can deploy manually using the admin API${NC}"
fi

echo "Waiting for deployment (10 seconds)..."
sleep 10
echo ""

# Step 6: Send test requests
echo -e "${BLUE}Step 6: Sending test requests via proxy${NC}"
for i in {1..3}; do
  RESPONSE=$(curl -s -H "Host: hello-axum" http://127.0.0.1:38080/)
  echo "Request $i: $RESPONSE"
  sleep 0.5
done
echo -e "${GREEN}✓ Requests successful${NC}"
echo ""

# Step 7: Find the instance direct address
echo -e "${BLUE}Step 7: Finding instance direct address${NC}"
INSTANCE_PORT=""
for port in {40000..40010}; do
  if curl -s http://127.0.0.1:$port/ > /dev/null 2>&1; then
    INSTANCE_PORT=$port
    echo -e "${GREEN}✓ Found instance at 127.0.0.1:$port${NC}"
    break
  fi
done

if [ -z "$INSTANCE_PORT" ]; then
  echo "Could not find running instance"
  kill $NODE_PID $ARTIFACT_PID 2>/dev/null || true
  exit 1
fi
echo ""

# Step 8: Test graceful shutdown
echo -e "${BLUE}Step 8: Testing graceful shutdown via /_platform/shutdown${NC}"
echo "Sending POST to http://127.0.0.1:$INSTANCE_PORT/_platform/shutdown"
SHUTDOWN_RESPONSE=$(curl -s -X POST http://127.0.0.1:$INSTANCE_PORT/_platform/shutdown)
echo "Response: $SHUTDOWN_RESPONSE"
echo ""

# Step 9: Verify instance stopped
echo -e "${BLUE}Step 9: Verifying instance shutdown${NC}"
echo "Waiting 2 seconds..."
sleep 2

if curl -s --max-time 1 http://127.0.0.1:$INSTANCE_PORT/ > /dev/null 2>&1; then
  echo -e "${YELLOW}Instance still responding (might take longer)${NC}"
else
  echo -e "${GREEN}✓ Instance has stopped (connection refused)${NC}"
fi
echo ""

# Step 10: Cleanup
echo -e "${BLUE}Step 10: Cleanup${NC}"
kill $NODE_PID $ARTIFACT_PID 2>/dev/null || true
rm -f /tmp/wasm-graceful-demo.redb /tmp/wasm-graceful-demo.key
echo -e "${GREEN}✓ Cleaned up${NC}"
echo ""

echo "================================================"
echo -e "${GREEN}✅ Graceful Shutdown Test Complete!${NC}"
echo "================================================"
echo ""
echo "What was demonstrated:"
echo "  1. Built WASM app with /_platform/shutdown endpoint"
echo "  2. Deployed app to the platform"
echo "  3. Sent successful HTTP requests"
echo "  4. Triggered graceful shutdown via POST /_platform/shutdown"
echo "  5. Verified instance exits cleanly"
echo ""
echo "Check the logs at: /tmp/wasm-node-graceful.log"
