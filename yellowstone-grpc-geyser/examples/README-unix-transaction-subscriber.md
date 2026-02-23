# Unix Transaction Subscriber Example

这个示例演示如何使用Unix域套接字订阅特定Solana账户的交易。

## 功能特性

- 🔌 **Unix域套接字连接**: 高性能本地连接
- 🎯 **精确账户过滤**: 只订阅涉及特定账户的交易
- 📊 **实时交易监控**: 实时显示交易详情
- 🚫 **智能过滤**: 排除投票交易和失败交易
- 📋 **详细信息**: 显示签名、slot、费用、指令等

## 目标账户

默认订阅账户: `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`

这是一个示例账户地址，你可以修改源码中的 `TARGET_ACCOUNT` 常量来监控其他账户。

## 编译

```bash
cd yellowstone-grpc-geyser
cargo build --example unix_transaction_subscriber --release
```

## 使用方法

### 1. 确保geyser插件运行

首先确保yellowstone-grpc-geyser插件在Solana验证器中运行，并配置了Unix套接字：

```bash
# 使用包含Unix套接字的配置
solana-validator \
  --geyser-plugin-config /path/to/geyser-config-unix.json \
  [其他验证器选项...]
```

### 2. 运行订阅客户端

```bash
# 使用默认Unix套接字路径
./target/release/examples/unix_transaction_subscriber /tmp/yellowstone-grpc.sock

# 或使用自定义路径
./target/release/examples/unix_transaction_subscriber /path/to/your/socket
```

## 输出示例

```
🚀 Starting Transaction Subscriber via Unix Socket
📡 Socket path: /tmp/yellowstone-grpc.sock
🎯 Target account: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA
✅ Target account validated: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA
🔍 Testing connection...
✅ Connection successful! Ping count: 1
📊 Starting transaction subscription...
🔧 Setting up transaction subscription filter...
📡 Sending subscription request...
🎉 Subscription established! Waiting for transactions...
📋 Filter: account_include=[pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA], vote=false, failed=false

🔥 Transaction #1: 5K7...abc
   📍 Slot: 123456789
   🗳️  Vote: false
   💰 Accounts involved: 8
   🎯 Target account found at index: 2
   📝 Instructions: 3
   💸 Fee: 5000 lamports

💓 Ping received (uptime: 30.5s, txs: 1)

🔥 Transaction #2: 7H9...def
   📍 Slot: 123456790
   🗳️  Vote: false
   💰 Accounts involved: 12
   🎯 Target account found at index: 5
   📝 Instructions: 2
   💸 Fee: 10000 lamports
```

## 配置选项

### 修改目标账户

编辑 `examples/unix_transaction_subscriber.rs` 文件中的常量：

```rust
const TARGET_ACCOUNT: &str = "你的账户地址";
```

### 调整过滤器

在 `subscribe_to_transactions` 函数中修改过滤器：

```rust
SubscribeRequestFilterTransactions {
    vote: Some(false),                    // 是否包含投票交易
    failed: Some(false),                  // 是否包含失败交易
    signature: None,                      // 特定签名过滤
    account_include: vec![target_account.to_string()], // 包含的账户
    account_exclude: vec![],              // 排除的账户
    account_required: vec![target_account.to_string()], // 必须包含的账户
}
```

### 调整承诺级别

```rust
commitment: Some(CommitmentLevel::Confirmed as i32), // Processed, Confirmed, Finalized
```

## 日志级别

设置环境变量来控制日志详细程度：

```bash
# 基础信息
RUST_LOG=info ./target/release/examples/unix_transaction_subscriber /tmp/yellowstone-grpc.sock

# 详细调试信息（包含完整JSON）
RUST_LOG=debug ./target/release/examples/unix_transaction_subscriber /tmp/yellowstone-grpc.sock

# 只显示错误
RUST_LOG=error ./target/release/examples/unix_transaction_subscriber /tmp/yellowstone-grpc.sock
```

## 性能优势

使用Unix域套接字相比TCP连接的优势：

- ⚡ **更低延迟**: 避免网络栈开销
- 🚀 **更高吞吐量**: 直接内核通信
- 🔒 **更好安全性**: 本地文件系统权限控制
- 💾 **更少资源消耗**: 无需网络缓冲

## 故障排除

### 连接失败

```bash
# 检查套接字文件是否存在
ls -la /tmp/yellowstone-grpc.sock

# 检查权限
sudo chmod 666 /tmp/yellowstone-grpc.sock

# 检查geyser插件日志
tail -f /path/to/validator/logs/validator.log | grep geyser
```

### 没有收到交易

1. 确认目标账户地址正确
2. 检查账户是否有活动
3. 验证geyser插件配置
4. 检查承诺级别设置

### 性能问题

1. 使用 `Processed` 承诺级别获得最快更新
2. 添加更多过滤器减少不相关交易
3. 考虑使用多个客户端分担负载

## 相关文件

- `unix_socket_test_client.rs` - 基础Unix套接字测试客户端
- `unix_socket_server_test.rs` - 独立测试服务器
- `config-unix.json` - Unix套接字配置示例
- `deploy/geyser-config-unix.json` - 生产配置示例
