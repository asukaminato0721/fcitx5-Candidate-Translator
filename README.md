# fcitx5 Candidate Translator

给 fcitx5 双拼候选词追加英语或日语翻译。插件只修改候选词的显示注释，实际提交内容、候选顺序和双拼的选词、翻页、删词等行为保持不变。

当前版本针对 fcitx5 5.1.x、`shuangpin` 输入法和 OpenAI Chat Completions 兼容接口。候选翻译会发送到你配置的远程服务；密码和敏感输入框不会发送请求。

## 构建与安装

需要 Rust、C++20 编译器、`pkg-config`、fcitx5 开发文件以及网络请求所需的 TLS 根证书。

```bash
cargo test --locked
make build
sudo make install
fcitx5 -r
```

Arch Linux 也可以从当前工作区构建本地包：

```bash
cd packaging
makepkg -si
```

卸载手工安装的文件：

```bash
sudo make uninstall
fcitx5 -r
```

## 配置

打开 `fcitx5-configtool`，在“附加组件”中找到“候选词翻译”，填写：

- `Base URL`：兼容接口的 `/v1` 地址，例如 `https://api.example.com/v1`。插件会追加 `/chat/completions`。
- `Model`：服务支持的模型名。
- `API Key`：Bearer Token。
- `Target language`：English 或 Japanese。

三个接口字段默认留空，未填写完整前不会发送请求。API Key 按需求明文保存在 `~/.config/fcitx5/conf/candidate-translator.conf`；插件会尽力将该文件权限收紧为 `0600`，但仍不建议在共享账户中使用长期高权限密钥。

翻译缓存保存在 `~/.cache/fcitx5/candidate-translator/cache-v1.json`，不包含 API Key。配置页勾选 `Clear translation cache on Apply` 后应用，可以清空缓存。

## 接口约定

插件批量发送当前页中包含汉字的候选词。模型必须只返回以下 JSON 结构，外层允许 Markdown JSON 代码块：

```json
{
  "translations": [
    {"index": 0, "text": "recite words"},
    {"index": 1, "text": "bed sheet"}
  ]
}
```

请求异步执行并带有防抖。超时、HTTP 错误或无效响应只会暂时保留原候选，不会阻塞输入。

## 开发检查

```bash
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
nm -D target/release/libfcitx5_candidate_translator.so | grep fcitx_addon_factory_instance
```
