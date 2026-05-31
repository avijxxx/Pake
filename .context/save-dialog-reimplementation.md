# Save Dialog 功能重新实现

## 日期

2026-05-31

## 改动概述

将下载功能的保存对话框从 **blocking API** 重新实现为 **异步 API**，使其更符合 Tauri 的最佳实践。

## 主要改动

### 1. `src-tauri/src/app/invoke.rs`

**改动前（blocking 方式）：**

```rust
// Show save dialog (requires `blocking` feature on tauri-plugin-dialog)
let file_path = app
    .dialog()
    .file()
    .add_filter("All Files", &["*"])
    .set_file_name(&params.filename)
    .set_directory(&download_dir)
    .blocking_save_file();
```

**改动后（异步方式）：**

```rust
// Show save dialog asynchronously
let file_path = app
    .dialog()
    .file()
    .add_filter("All Files", &["*"])
    .set_file_name(&params.filename)
    .set_directory(&download_dir)
    .save_file()
    .await;
```

**其他清理：**

- 移除了临时的 debug 日志代码
- 简化了取消对话框的处理逻辑

### 2. `src-tauri/Cargo.toml`

**改动前：**

```toml
tokio = { version = "1.49.0", features = ["time", "sync"] }
tauri-plugin-dialog = { version = "2", features = ["blocking"] }
```

**改动后：**

```toml
tokio = { version = "1.49.0", features = ["time"] }
tauri-plugin-dialog = "2"
```

**说明：**

- 移除了 `tauri-plugin-dialog` 的 `blocking` feature
- 移除了 `tokio` 的 `sync` feature（代码中未使用）

## 技术优势

### 异步方式的优点：

1. **非阻塞**：不会阻塞 Tauri 的主线程，UI 响应更流畅
2. **符合最佳实践**：Tauri 推荐使用异步 API
3. **更简洁**：不需要额外的 `blocking` feature
4. **更好的性能**：与 Tauri 的异步运行时更好地集成

### Blocking 方式的问题：

1. 会阻塞当前线程，可能导致 UI 卡顿
2. 需要额外的依赖 feature
3. 在某些平台上可能有兼容性问题

## 功能保持不变

- 用户点击下载链接时弹出保存对话框
- 默认文件名为原始文件名
- 默认保存位置为系统下载文件夹
- 用户可以选择任意位置保存
- 用户可以取消下载
- 文件名冲突时自动追加 `-N` 后缀
- 下载过程中显示 Toast 通知（开始、成功、失败）

## 测试建议

1. 测试正常下载流程
2. 测试取消对话框
3. 测试文件名冲突处理
4. 测试不同文件类型的下载
5. 测试大文件下载
6. 测试网络错误处理

## 相关 Commit

- 原始实现：`d0ee3ec` - feat: add save dialog for downloads
- 本次重新实现：使用异步 API 替代 blocking API
