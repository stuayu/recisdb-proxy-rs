# BonDriver ネットワークプロキシシステム実装計画

## 概要

recisdb-rsを拡張し、ネットワーク経由でTSストリームを配信するサーバーと、BonDriver互換DLLクライアントを実装する。

## 要件

- **TCP通信**: パケットロスなし
- **低遅延**: ロックフリーリングバッファ採用
- **BonDriver v1/v2/v3 互換**: 全インターフェース実装
- **チャンネル共有**: 同一チャンネルは1チューナーを複数クライアントで共有
- **クロスプラットフォーム**: サーバーはLinux/Windows両対応
- **TLS認証**: 証明書ベースのクライアント認証

## システムアーキテクチャ

```
┌─────────────────────────────────────────────────────────────────┐
│  CLIENT (Windows)                                               │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  BonDriver_NetworkProxy.dll                               │  │
│  │  - IBonDriver/2/3 インターフェース実装                      │  │
│  │  - TCPクライアント (tokio)                                 │  │
│  │  - リングバッファ (2MB, ロックフリー)                       │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
                              │ TCP
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│  SERVER (Rust)                                                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  recisdb-proxy-server                                     │  │
│  │  - TCPリスナー (tokio)                                     │  │
│  │  - セッション管理                                          │  │
│  │  - TunerPool (チャンネル共有ロジック)                       │  │
│  │  - broadcast::Sender でTS配信                             │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              │                                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  既存チューナーバックエンド                                 │  │
│  │  - BonDriver (Windows)                                    │  │
│  │  - Character Device (Linux)                               │  │
│  │  - DVBv5 (Linux)                                          │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

## 新規クレート構成

```
recisdb-rs/
├── Cargo.toml                    # ワークスペースに追加
├── recisdb-protocol/             # 共通プロトコル定義
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── types.rs              # メッセージ型
│       ├── codec.rs              # エンコード/デコード
│       └── error.rs
├── recisdb-proxy/                # サーバー
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       ├── server/
│       │   ├── listener.rs       # TCP Accept
│       │   └── session.rs        # クライアントセッション
│       └── tuner/
│           ├── pool.rs           # TunerPool管理
│           ├── shared.rs         # SharedTuner (broadcast)
│           └── channel_key.rs    # チャンネル識別キー
└── bondriver-proxy-client/       # クライアントDLL
    ├── Cargo.toml
    ├── build.rs
    └── src/
        ├── lib.rs                # DLLエントリ + CreateBonDriver
        ├── bondriver/
        │   ├── interface.rs      # vtable構造体
        │   └── exports.rs        # C呼び出し可能関数
        └── client/
            ├── connection.rs     # TCP接続管理
            └── buffer.rs         # TSリングバッファ
```

## プロトコル仕様

### フレームフォーマット
```
┌──────────┬──────────┬─────────────┬────────────────────┐
│  Magic   │  Length  │   Type      │      Payload       │
│  "BNDP"  │  u32 LE  │   u16 LE    │     (variable)     │
└──────────┴──────────┴─────────────┴────────────────────┘
```

### 主要メッセージタイプ
| Type | 名称 | 方向 | 用途 |
|------|------|------|------|
| 0x0001 | OpenTuner | C→S | チューナーオープン要求 |
| 0x0101 | SetChannel | C→S | チャンネル設定 |
| 0x0102 | SetChannelSpace | C→S | Space/Channel設定 |
| 0x0301 | GetSignalLevel | C→S | 信号レベル取得 |
| 0x0401 | StartStream | C→S | ストリーム開始 |
| 0x0403 | StreamData | S→C | TSデータ送信 |

## チャンネル共有ロジック

```rust
pub struct TunerPool {
    tuners: RwLock<HashMap<ChannelKey, Arc<SharedTuner>>>,
}

impl TunerPool {
    pub async fn get_or_create(&self, tuner_path: &str, channel: &Channel)
        -> Result<Arc<SharedTuner>>
    {
        let key = ChannelKey::from(tuner_path, channel);

        // 既存チューナーがあれば再利用
        if let Some(tuner) = self.tuners.read().await.get(&key) {
            return Ok(Arc::clone(tuner));
        }

        // なければ新規作成
        let shared = self.create_tuner(tuner_path, channel).await?;
        self.tuners.write().await.insert(key, Arc::clone(&shared));
        Ok(shared)
    }
}
```

## リングバッファ設計

```rust
const RING_BUFFER_SIZE: usize = 2 * 1024 * 1024; // 2MB

pub struct TsRingBuffer {
    buffer: Box<[u8; RING_BUFFER_SIZE]>,
    write_pos: AtomicUsize,  // Receiver Task が更新
    read_pos: AtomicUsize,   // Main Thread (GetTsStream) が更新
}
```

## 主要参照ファイル

| ファイル | 参照理由 |
|----------|----------|
| `recisdb-rs/src/tuner/windows/IBonDriver.hpp` | BonDriverインターフェース定義 |
| `recisdb-rs/src/tuner/windows/mod.rs` | AsyncRead実装パターン |
| `recisdb-rs/src/io.rs` | ストリーミングパイプライン |
| `recisdb-rs/src/channels.rs` | チャンネル型定義 |

## 実装フェーズ

### Phase 1: プロトコル基盤
- `recisdb-protocol` クレート作成
- メッセージ型・コーデック実装
- ユニットテスト

### Phase 2: サーバーコア
- `recisdb-proxy` クレート作成
- TunerPool実装
- TCPリスナー・セッション管理

### Phase 3: クライアントコア
- `bondriver-proxy-client` クレート作成
- リングバッファ実装
- IBonDriver vtable実装

### Phase 4: 統合・テスト
- 全IBonDriverメソッド実装
- 設定ファイルサポート
- TVTest等での動作確認

## TLS認証設計

### 証明書構成
```
certs/
├── ca.crt              # CA証明書（サーバー・クライアント共通）
├── server.crt          # サーバー証明書
├── server.key          # サーバー秘密鍵
├── client.crt          # クライアント証明書
└── client.key          # クライアント秘密鍵
```

### サーバー側TLS設定
```rust
use tokio_rustls::TlsAcceptor;
use rustls::{ServerConfig, Certificate, PrivateKey};

let config = ServerConfig::builder()
    .with_safe_defaults()
    .with_client_cert_verifier(
        AllowAnyAuthenticatedClient::new(root_cert_store)
    )
    .with_single_cert(server_certs, server_key)?;

let acceptor = TlsAcceptor::from(Arc::new(config));
```

### クライアント側TLS設定
```rust
use tokio_rustls::TlsConnector;
use rustls::{ClientConfig, Certificate, PrivateKey};

let config = ClientConfig::builder()
    .with_safe_defaults()
    .with_root_certificates(root_cert_store)
    .with_client_auth_cert(client_certs, client_key)?;

let connector = TlsConnector::from(Arc::new(config));
```

### 設定ファイル拡張

**サーバー (recisdb-proxy.toml)**
```toml
[tls]
enabled = true
ca_cert = "/etc/recisdb-proxy/certs/ca.crt"
server_cert = "/etc/recisdb-proxy/certs/server.crt"
server_key = "/etc/recisdb-proxy/certs/server.key"
require_client_cert = true
```

**クライアント (BonDriver_NetworkProxy.ini)**
```ini
[TLS]
Enabled=1
CaCert=ca.crt
ClientCert=client.crt
ClientKey=client.key
```

## クロスプラットフォーム対応

### サーバー側チューナー抽象化
```rust
// tuner/backend.rs
pub enum TunerBackend {
    #[cfg(target_os = "linux")]
    CharacterDevice(character_device::Tuner),
    #[cfg(target_os = "linux")]
    DvbV5(dvbv5::Tuner),
    #[cfg(target_os = "windows")]
    BonDriver(windows::Tuner),
}

impl TunerBackend {
    pub async fn open(path: &str) -> Result<Self, TunerError> {
        #[cfg(target_os = "linux")]
        {
            if path.contains("/dev/dvb/") {
                return Ok(Self::DvbV5(dvbv5::Tuner::new(path)?));
            }
            return Ok(Self::CharacterDevice(character_device::Tuner::new(path)?));
        }
        #[cfg(target_os = "windows")]
        {
            return Ok(Self::BonDriver(windows::Tuner::new(path)?));
        }
    }
}
```

### 依存関係 (Cargo.toml)
```toml
[dependencies]
tokio-rustls = "0.25"
rustls = "0.22"
rustls-pemfile = "2"

[target.'cfg(unix)'.dependencies]
nix = { version = "0.28", features = ["ioctl"] }
dvbv5 = { version = "0.1", optional = true }

[target.'cfg(windows)'.dependencies]
libloading = "0.8"
```

## 検証方法

1. **サーバー起動テスト**
   ```bash
   cargo run -p recisdb-proxy -- --listen 0.0.0.0:12345 --tuner /dev/pt3video0
   ```

2. **クライアントDLLビルド**
   ```bash
   cargo build -p bondriver-proxy-client --release --target x86_64-pc-windows-msvc
   ```

3. **TVTestで動作確認**
   - BonDriver_NetworkProxy.dll をTVTestのBonDriverフォルダに配置
   - BonDriver_NetworkProxy.ini でサーバーアドレス設定
   - TVTestでチャンネル選局・視聴確認

4. **チャンネル共有テスト**
   - 複数TVTestインスタンスで同一チャンネルを選局
   - サーバーログでチューナー共有を確認
