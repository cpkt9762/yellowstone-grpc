# Yellowstone gRPC Geyser Plugin - Linux Deployment Guide

## 🎉 Features

This yellowstone-grpc-geyser plugin supports:

- **TCP connections**: Traditional network-based gRPC connections
- **Unix domain sockets**: High-performance local connections
- **Dual mode**: Both TCP and Unix socket simultaneously
- **Real-time Solana data streaming**: Accounts, slots, transactions, blocks
- **Compression support**: gzip and zstd compression
- **Flexible configuration**: Extensive configuration options

## 📦 Files

### Plugin Binary
- `libyellowstone_grpc_geyser.so` - The main plugin library (8.1MB)

### Configuration Examples
- `geyser-config-tcp.json` - TCP-only configuration
- `geyser-config-unix.json` - Unix socket-only configuration  
- `geyser-config-dual.json` - Both TCP and Unix socket configuration

### Test Clients
- `unix_socket_test_client` - Test client for both TCP and Unix socket connections

## 🚀 Deployment Steps

### 1. Upload Files to Linux Server

```bash
# Upload the plugin library
scp libyellowstone_grpc_geyser.so user@server:/opt/solana/plugins/

# Upload configuration files
scp geyser-config-*.json user@server:/opt/solana/config/

# Upload test client (optional)
scp unix_socket_test_client user@server:/opt/solana/bin/
```

### 2. Update Configuration

Edit the configuration file and update the `libpath`:

```json
{
  "libpath": "/opt/solana/plugins/libyellowstone_grpc_geyser.so",
  "grpc": {
    // ... rest of configuration
  }
}
```

### 3. Start Solana Validator

#### TCP Mode
```bash
solana-validator \
  --identity /path/to/validator-keypair.json \
  --vote-account /path/to/vote-account-keypair.json \
  --ledger /path/to/ledger \
  --rpc-port 8899 \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --limit-ledger-size \
  --geyser-plugin-config /opt/solana/config/geyser-config-tcp.json \
  --log -
```

#### Unix Socket Mode
```bash
solana-validator \
  --identity /path/to/validator-keypair.json \
  --vote-account /path/to/vote-account-keypair.json \
  --ledger /path/to/ledger \
  --rpc-port 8899 \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --limit-ledger-size \
  --geyser-plugin-config /opt/solana/config/geyser-config-unix.json \
  --log -
```

#### Dual Mode (TCP + Unix Socket)
```bash
solana-validator \
  --identity /path/to/validator-keypair.json \
  --vote-account /path/to/vote-account-keypair.json \
  --ledger /path/to/ledger \
  --rpc-port 8899 \
  --entrypoint entrypoint.mainnet-beta.solana.com:8001 \
  --limit-ledger-size \
  --geyser-plugin-config /opt/solana/config/geyser-config-dual.json \
  --log -
```

### 4. Verify Plugin Loading

Look for these log messages:
```
[INFO] gRPC server listening on TCP address: 0.0.0.0:10000
[INFO] gRPC server listening on Unix socket: "/tmp/yellowstone-grpc.sock"
```

### 5. Test Connections

#### Test TCP Connection
```bash
# Using the test client
./unix_socket_test_client tcp://127.0.0.1:10000

# Or using grpcurl
grpcurl -plaintext 127.0.0.1:10000 geyser.Geyser/Ping
```

#### Test Unix Socket Connection
```bash
# Using the test client
./unix_socket_test_client /tmp/yellowstone-grpc.sock

# Check socket file exists
ls -la /tmp/yellowstone-grpc.sock
```

## 🔧 Configuration Options

### Connection Settings
- `address`: TCP address (e.g., "0.0.0.0:10000")
- `unix_socket_path`: Unix socket path (e.g., "/tmp/yellowstone-grpc.sock")

### Performance Settings
- `channel_capacity`: Message channel capacity (default: 100000)
- `unary_concurrency_limit`: Concurrent unary requests (default: 100)
- `max_decoding_message_size`: Max message size in bytes (default: 4MB)

### Compression Settings
- `compression.accept`: Accepted compression formats ["gzip", "zstd"]
- `compression.send`: Compression format for sending data

### Filter Settings
- `filter_name_size_limit`: Max filter name size (default: 128)
- `filter_names_size_limit`: Max total filter names size (default: 4096)
- `filter_names_cleanup_interval`: Cleanup interval in ms (default: 1000)

## 🔍 Monitoring

### Check Plugin Status
```bash
# Check if socket exists (Unix mode)
ls -la /tmp/yellowstone-grpc.sock

# Check if TCP port is listening
netstat -tlnp | grep :10000

# Check validator logs
tail -f /path/to/validator.log | grep -i geyser
```

### Performance Monitoring
```bash
# Monitor connections
ss -tuln | grep 10000  # TCP connections
ss -xl | grep yellowstone  # Unix socket connections

# Monitor resource usage
top -p $(pgrep solana-validator)
```

## 🚨 Troubleshooting

### Common Issues

1. **Plugin not loading**
   - Check file permissions: `chmod +x libyellowstone_grpc_geyser.so`
   - Verify path in config file
   - Check validator logs for error messages

2. **Unix socket permission denied**
   - Check socket file permissions
   - Ensure validator process can create/write to socket path
   - Try different socket path: `/var/run/yellowstone-grpc.sock`

3. **TCP port already in use**
   - Change port in configuration
   - Check for conflicting services: `netstat -tlnp | grep :10000`

4. **High memory usage**
   - Reduce `channel_capacity`
   - Adjust `filter_names_size_limit`
   - Monitor client connection count

### Log Analysis
```bash
# Filter geyser-related logs
journalctl -u solana-validator | grep -i geyser

# Monitor real-time logs
tail -f /var/log/solana-validator.log | grep -E "(gRPC|client|account|slot)"
```

## 📊 Performance Comparison

| Connection Type | Latency | Throughput | Security | Use Case |
|----------------|---------|------------|----------|----------|
| TCP | Higher | Good | Network-level | Remote clients |
| Unix Socket | Lower | Excellent | Local-only | Local applications |
| Dual Mode | Mixed | Best | Flexible | Mixed environments |

## 🎯 Production Recommendations

1. **Use Unix sockets for local clients** (better performance)
2. **Use TCP for remote clients** (network accessibility)
3. **Use dual mode for mixed environments** (maximum flexibility)
4. **Monitor connection counts and resource usage**
5. **Set appropriate filter limits based on your use case**
6. **Use compression for high-throughput scenarios**

## 📝 Notes

- Plugin compiled with Rust 1.x for x86_64-unknown-linux-gnu
- Compatible with Solana validator v1.18+
- Supports all standard gRPC features (streaming, compression, etc.)
- Thread-safe and production-ready
