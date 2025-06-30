#!/bin/bash

# Yellowstone gRPC Geyser Plugin Deployment Script
# Usage: ./deploy.sh [server_address] [deployment_path]

set -e

# Default values
SERVER_ADDRESS="${1:-user@your-server.com}"
DEPLOYMENT_PATH="${2:-/opt/solana}"
PLUGIN_PATH="${DEPLOYMENT_PATH}/plugins"
CONFIG_PATH="${DEPLOYMENT_PATH}/config"
BIN_PATH="${DEPLOYMENT_PATH}/bin"

echo "🚀 Deploying Yellowstone gRPC Geyser Plugin"
echo "   Server: $SERVER_ADDRESS"
echo "   Deployment path: $DEPLOYMENT_PATH"
echo ""

# Check if files exist
if [ ! -f "linux-x86_64/libyellowstone_grpc_geyser.so" ]; then
    echo "❌ Plugin library not found. Please run 'cargo zigbuild --target x86_64-unknown-linux-gnu --release' first."
    exit 1
fi

if [ ! -f "linux-x86_64/unix_socket_test_client" ]; then
    echo "❌ Test client not found. Please run 'cargo zigbuild --target x86_64-unknown-linux-gnu --release --example unix_socket_test_client' first."
    exit 1
fi

echo "📦 Creating deployment directories on server..."
ssh "$SERVER_ADDRESS" "sudo mkdir -p $PLUGIN_PATH $CONFIG_PATH $BIN_PATH"

echo "📤 Uploading plugin library..."
scp linux-x86_64/libyellowstone_grpc_geyser.so "$SERVER_ADDRESS:$PLUGIN_PATH/"

echo "📤 Uploading configuration files..."
scp geyser-config-*.json "$SERVER_ADDRESS:$CONFIG_PATH/"

echo "📤 Uploading test client..."
scp linux-x86_64/unix_socket_test_client "$SERVER_ADDRESS:$BIN_PATH/"

echo "📤 Uploading documentation..."
scp README.md "$SERVER_ADDRESS:$DEPLOYMENT_PATH/"

echo "🔧 Setting permissions..."
ssh "$SERVER_ADDRESS" "
    sudo chmod +x $PLUGIN_PATH/libyellowstone_grpc_geyser.so
    sudo chmod +x $BIN_PATH/unix_socket_test_client
    sudo chown -R solana:solana $DEPLOYMENT_PATH 2>/dev/null || true
"

echo "📝 Updating configuration files..."
ssh "$SERVER_ADDRESS" "
    sudo sed -i 's|/path/to/libyellowstone_grpc_geyser.so|$PLUGIN_PATH/libyellowstone_grpc_geyser.so|g' $CONFIG_PATH/geyser-config-*.json
"

echo ""
echo "✅ Deployment completed successfully!"
echo ""
echo "📋 Next steps:"
echo "1. Choose a configuration file:"
echo "   - TCP only: $CONFIG_PATH/geyser-config-tcp.json"
echo "   - Unix socket only: $CONFIG_PATH/geyser-config-unix.json"
echo "   - Both TCP and Unix: $CONFIG_PATH/geyser-config-dual.json"
echo ""
echo "2. Start your Solana validator with the geyser plugin:"
echo "   solana-validator \\"
echo "     --geyser-plugin-config $CONFIG_PATH/geyser-config-dual.json \\"
echo "     [other validator options...]"
echo ""
echo "3. Test the connection:"
echo "   # TCP test"
echo "   $BIN_PATH/unix_socket_test_client tcp://127.0.0.1:10000"
echo ""
echo "   # Unix socket test"
echo "   $BIN_PATH/unix_socket_test_client /tmp/yellowstone-grpc.sock"
echo ""
echo "📖 For detailed instructions, see: $DEPLOYMENT_PATH/README.md"
