# drydb-rs

[DryDB](https://github.com/hadashiA/DryDB) 1.4 のファイル形式を読み書きする、Rust製の読み取り専用組み込みKVストア。

データベースは一度だけビルドして不変のファイルにする。実行時は問い合わせが触れるB+Treeのページだけを読むので、起動時に全件をデシリアライズすることも、索引を作り直すことも、ファイル全体をメモリに載せることもない。SQL、実行時の更新、トランザクション、WAL、MVCCは対象外。

English: [README.md](README.md)

## DryDBとの関係

DryDBは [hadashiA](https://github.com/hadashiA) 氏がC#で書いた読み取り専用の組み込みデータベース。このプロジェクトは同じファイルを読み書きするので、どちらの実装が作ったファイルでも、もう一方が読める。

対象は upstream commit `6b175929491793948e63430c20c2d6f58300d97f` の storage format 1.4。DryDBのソースコードはこのリポジトリに取り込んでいない。形式はupstreamのソースとドキュメントを読んで実装し、相互運用テストのために `tests/interop/fetch-upstream.sh` がその固定commitをgit管理外のディレクトリにcloneして、同じfixtureで両方の実装を走らせる。挙動が食い違う箇所は、upstream側の実測値とともに[例外台帳](docs/compatibility.md#6-例外台帳)に記録してある。

## 導入

crates.ioには公開していない。リポジトリから取得する。

```toml
[dependencies]
drydb = { git = "https://github.com/hckaye/drydb-rs" }
```

既定featureでは `drydb` に依存crateは無い。optionalなfeatureが2つある。

| feature | 内容 |
| --- | --- |
| `mmap` | メモリマップのページソース。ページはmappingからバッファへコピーするので、mapping内部への参照は外に出ない。ただし `MmapSource::open` は `unsafe` で、開いているあいだファイルが変更されないことの保証は呼び出し側の責任になり、OSがresidentにしたページはメモリ予算の対象外になる。 |
| `zstd` | `DryDB.ZstdCompression` のページフィルタ。upstreamの圧縮を使ったファイルを読み書きできる。 |

| crate | 内容 |
| --- | --- |
| [`drydb`](crates/drydb) | 形式の復号、page I/O、ページキャッシュ、B+Tree検索、cursor、副索引、blob、builder、`verify` |
| [`drydb-rkyv`](crates/drydb-rkyv) | レコード単位のrkyv値codec |
| [`drydb-msgpack`](crates/drydb-msgpack) | MessagePack値のcodec。MessagePack-CSharpと同じbytesを読み書きする |
| [`drydb-async`](crates/drydb-async) | tokioのblocking poolを使うasync adapter |
| [`drydb-cli`](crates/drydb-cli) | `drydb` コマンド |

## ファイルを作る

行は順不同で渡してよい。builderが並べ替え、ソート用バッファに収まらなくなった分を一時ファイルへ書き出し、木を1パスで書く。

```rust
use std::sync::Arc;

use drydb::{AsciiEncoding, DatabaseBuilder, Int64Encoding};

let mut builder = DatabaseBuilder::new().page_size(4096)?;
let items = builder.create_table("items", Arc::new(Int64Encoding))?;
builder.add_secondary_index(
    items,
    "by_name",
    false,
    Arc::new(AsciiEncoding),
    Box::new(|_key, value| Ok(value.to_vec())),
)?;

for id in 0..1_000i64 {
    let name = format!("item-{id:04}");
    builder.append(items, &Int64Encoding::encode(id), name.as_bytes())?;
}

// 葉ページに収まらない大きさの値を置く2つ目のtable。
let assets = builder.create_table("assets", Arc::new(Int64Encoding))?;
for id in 0..4i64 {
    builder.append(assets, &Int64Encoding::encode(id), &vec![b'x'; 200 * 1024])?;
}

let report = builder.build_to_file("game.drydb")?;
println!("{} pages, {} bytes", report.page_count, report.file_size);
```

`build_to_vec` は同じファイルを書き出さずにbytesで返す。

キーのencodingが、順序と、検索が比較する64 bitのdigestを決める。`Int64Encoding`、`AsciiEncoding`、`Uuidv7Encoding`、`UlidEncoding` が同梱されていて、`KeyEncoding` traitを実装すれば独自のものも使える。

## ファイルを読む

### 1件取得

```rust
use drydb::{Database, Int64Encoding};

let db = Database::open("game.drydb")?;
let table = db.table("items")?;

if let Some(value) = table.get(&Int64Encoding::encode(42))? {
    // キャッシュ上のページからの借用。guardがそのページを保持する。
    println!("{} bytes", value.as_bytes().len());
}
```

`ValueGuard` は自分でページを保持する。キャッシュからの退避、他の問い合わせ、`Database` のdropは、手元のbyte列を無効にしない。

### 範囲、prefix、件数

```rust
use std::ops::Bound;

use drydb::Order;

let mut cursor = table.range(
    Bound::Included(&Int64Encoding::encode(100)[..]),
    Bound::Excluded(&Int64Encoding::encode(200)[..]),
    Order::Ascending,
)?;
while cursor.advance()? {
    let entry = cursor.current().expect("positioned");
    let _ = (entry.key(), entry.value());
}

let rows = table.count_range(Bound::Unbounded, Bound::Unbounded)?;
```

cursorが保持するのは、いま見ている葉ページなので、走査中に保持する量はテーブルの大きさに比例しない。ただし葉ページに収まらない値は専用のページに置かれ、その行に進んだ時点で値の全体を読む。走査には、通過するいちばん大きい値が入るだけの予算が要る。200 KBの値が並ぶテーブルなら走査もその分を要求し、`count` は何も要求せず、`BlobReader` はチャンク1個分で済む。`scan` はテーブル全体を、`prefix` は指定したbyte列で始まるキーを走査する。`count` と `count_range` は値を1件も読まない。

### 副索引

副索引は自分のキーから、主キーの木にある行への参照を持つ。副索引経由の読み取りは、この参照を解決して値を返す。

```rust
let index = table.index("by_name")?;
let mut cursor = index.lookup(b"item-0042")?;
while cursor.advance()? {
    let key = cursor.key();
    if let Some(value) = cursor.value() {
        let _ = (key, value.as_bytes());
    }
}
```

`get` はそのキーの最初の行を返す。非unique索引では、一致する行のうち主キーがいちばん小さいものになる。`lookup` は同じ順序で、そのキーの行をすべて返す。

### 大きすぎてメモリに置きたくない値

`Table::get` は値を丸ごとメモリに載せる。葉ページに収まらない値は専用のページに置かれるので、そういう値は指定したチャンクサイズのバッファだけで流し読みできる。この経路はファイルを直接読むため、ページフィルタの無いデータベースでのみ使える。

```rust
let assets = db.table("assets")?;
if let Some(reader) = assets.blob_reader(&Int64Encoding::encode(2))? {
    let mut out = Vec::new();
    let copied = reader.copy_to(&mut out, 64 * 1024)?;
    println!("{copied} bytes");
}
```

`BlobReader` は `Read` と `Seek` も実装している。キーが無ければ `None` を返し、葉ページ内に収まっている値は断る。そちらは `get` で読む。

### 型のある値

データベースから見れば値はbytesでしかない。型を与えるadapterが2つある。どちらも書き込みと読み出しで同じcodecを使うので、ここの例はそれぞれ自分でtableを作る。

```rust
use drydb_msgpack::{MessagePackCodec, MessagePackTable};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Item {
    name: String,
    price: u32,
}

let codec = MessagePackCodec::new();
let mut builder = DatabaseBuilder::new().page_size(4096)?;
let stock = builder.create_table("stock", Arc::new(Int64Encoding))?;
let value = codec.serialize(&Item { name: "sword".to_string(), price: 120 })?;
builder.append(stock, &Int64Encoding::encode(42), &value)?;
let db = Database::open_bytes(builder.build_to_vec()?)?;

let stock = MessagePackTable::new(db.table("stock")?, MessagePackCodec::new());
let item: Option<Item> = stock.get(&Int64Encoding::encode(42))?;
```

`drydb-rkyv` は、rkyvのarchiveがページ内でアラインされていれば、コピーせずにそのまま読む。アラインされていない場合はその値だけをコピーし、codecに渡したデータベースの予算に計上する。

```rust
use std::sync::Arc;

use drydb_rkyv::{Codec, RkyvSchema, SchemaId};

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct Monster {
    name: String,
    hp: u32,
}

impl RkyvSchema for Monster {
    // アプリケーションが決める値で、各値のenvelopeに入る。
    const SCHEMA_ID: SchemaId = SchemaId(0x4D4F_4E53_5445_5201);
    const SCHEMA_VERSION: u16 = 1;
}

let codec = Codec::new();
let mut builder = DatabaseBuilder::new().page_size(4096)?;
let monsters = builder.create_table("monsters", Arc::new(Int64Encoding))?;
let value = codec.serialize(&Monster { name: "slime".to_string(), hp: 12 })?;
builder.append(monsters, &Int64Encoding::encode(42), &value)?;
let db = Database::open_bytes(builder.build_to_vec()?)?;

// データベースを渡しておくと、アラインされていないarchiveのコピーが予算に計上される。
let codec = Codec::new().budget(Arc::clone(db.budget()));
let monsters = db.table("monsters")?;
if let Some(value) = monsters.get(&Int64Encoding::encode(42))? {
    let prepared = codec.prepare::<Monster>(value)?;
    let monster = prepared.access()?;
    println!("{}", monster.hp);
}
```

`Codec::new()` だけで作ったcodecには計上先の予算が無いので、コピーはデータベースの予算の外に出る。`Codec::reserve_with` には `Database` か `Table` を渡せて、こちらはキャッシュ上のページを回収して場所を空けてから確保する。

rkyv値はC#からは不透明なbytesとして扱う。MessagePack値はDTO単位で両側から型付きで読める。

### 非同期コードから

ページの読み込みはブロッキングのファイル読み込みで、adapterはそれを隠さない。必要なページがすべてキャッシュにあれば呼び出し元のスレッドで完了し、無ければ問い合わせ全体を `tokio::task::spawn_blocking` に渡す。

```rust
use drydb_async::AsyncDatabase;

let db = AsyncDatabase::open("game.drydb").await?;
let table = db.table("items")?;
let value = table.get(&Int64Encoding::encode(42)).await?;
```

## メモリ

`OpenOptions::memory_budget` が、このcrateが利用者のために確保するbyte数の上限になる。

```rust
use drydb::OpenOptions;

let db = OpenOptions::new()
    .memory_budget(8 * 1024 * 1024)
    .open("game.drydb")?;
```

確保するものは、確保する前にすべて予約する。ページバッファ、解析済みのcatalog、page directory、展開用バッファ、問い合わせが持つ境界のコピー、cursorのキーバッファ、一括取得が返す行がこれにあたる。収まらない要求は、キャッシュから回収できる分を回収したうえで `ErrorKind::BudgetExceeded` として断り、確保はしない。空きを待つ処理は無いので、予算が小さすぎればエラーになるだけで、デッドロックにはならない。

対象は `drydb` 本体で、返す値も含む。検証レポートやテーブル名の一覧は、それを確保した予約ごと返り、捨てたときに予算へ戻る。`drydb-async` と `drydb-msgpack` が返す行も同じ扱いになる。adapterは、予算を渡された場合にかぎり同じ予算に計上する。`drydb-rkyv` がアラインされていないarchiveのために作るコピーは、codecに `budget` か `reserve_with` を渡してから計上され、渡すまでは計上されない。復号した値が内部で確保する分は利用者の型のもので、計上の対象外になる。

この予算はRSSの上限ではない。allocator自身の内部費用、利用者が自分でコピーした値、OSのページキャッシュ、`mmap` のソースがresidentにしたページ、`Database::open_bytes` に渡したイメージは含まない。イメージは、diskのファイルと同じく読み取りの対象そのものなので、予算の外に置いている。`Database::memory_report` がこの区別を返す。

## ファイルを検査する

```rust
use drydb::VerifyOptions;

let report = db.verify(VerifyOptions::default())?;
if !report.is_ok() {
    for problem in &report.problems {
        println!("{problem:?}");
    }
}
```

検査はすべてのページとすべての木を歩き、そのあいだ保持するページ数には上限がある。予算不足や読み取りの失敗は、ファイルの問題ではなく `Err` として返る。ファイルが壊れていると分かったのではなく、調べられなかったということなので、扱いを分けている。

## コマンドライン

```sh
# `key<TAB>value` 形式の入力からファイルを作る
drydb build game.drydb --table items --encoding i64 --input rows.tsv

drydb inspect game.drydb
drydb verify game.drydb
drydb get game.drydb items 100
drydb range game.drydb items --from 100 --to 200 --limit 20
drydb count game.drydb items
drydb prefix names.drydb names ch
```

`get`、`range`、`count` は `--index <name>` を付けると、主キーではなく副索引を経由して読む。キーの表示と入力の形式は同じで、`--key-format` で選ぶ。一覧は `drydb --help` で見られる。

## この実装が保証するもの

**壊れた入力はエラーになる。** ファイルのbyte列をRustの構造体として再解釈することはなく、すべてのフィールドを、長さを検査したslice越しにlittle-endianから復号する。crate全体が `#![deny(unsafe_code)]` で、例外は `mmap` featureのmapping処理1か所だけ。壊れたファイルからは `CorruptData` が返り、panicにも未定義動作にもならず、一部だけの結果を全件として返すこともない。有効なファイルの1 byteを書き換えたものを多数作り、すべての読み取り経路に通すテストがこれを確認する。

**メモリは予算の内側に収まる。** 上の節のとおり。

**値は、それを取り出した問い合わせより長く生きられる。** `ValueGuard` は保持しているあいだ、自分のページをresidentに保つ。

## C#実装との互換性

相互運用は12件のfixtureで確認してある。i64 / ascii / uuidv7 / ulidのキー、classicとcompactのmetadata、Eytzinger digest、キーを省略するlayout、overflow値、unique / 非uniqueの副索引、zstd圧縮を覆う。各fixtureを両方の実装が生成し、両方が読んで、4通りの組合せを比較する。12件のうち10件は、両実装の出力がbyte単位で一致した。

食い違いは9件あり、すべて[例外台帳](docs/compatibility.md#6-例外台帳)に、upstream側の実測値をassertionとして固定したテストつきで記録してある。多くはupstreamのbuilderとreaderが互いに食い違っている箇所で、この実装は両方から読める側に倒してある。

## 開発

```sh
cargo test --workspace --exclude drydb-interop

# C#実装との比較。.NET SDKが必要。
./tests/interop/fetch-upstream.sh
DRYDB_INTEROP=1 cargo test -p drydb-interop

# 未定義動作の検査
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p drydb --lib
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p drydb --test miri_smoke

# fuzz。cargo-fuzzが必要。
cd fuzz && cargo +nightly fuzz run open-database

# 測定
cargo bench -p drydb --features zstd
```

MSRVは1.85。libraryと全featureとテスト一式が1.85でビルドできることを確認してある。

## 設計文書

| 文書 | 内容 |
| --- | --- |
| [アーキテクチャ](docs/architecture.md) | 所有権、ページキャッシュ、メモリ契約、I/O、安全性 |
| [互換性仕様](docs/compatibility.md) | wire形式、実測で確定した事項、対応範囲、例外台帳 |
| [rkyv codec](docs/rkyv-design.md) | envelope、profile検出、alignment、検証 |
| [ベンチマーク](docs/benchmarks.md) | 測定方法、基準値、採用した最適化と採用しなかったもの |
| [実装計画](docs/implementation-plan.md) | 到達状況、検証の構成、設計判断 |
| [タスクリスト](docs/tasks.md) | 各項目と対応するテストの場所 |

## ライセンスと出典

このリポジトリのライセンスは [MIT](LICENSE)、Copyright (c) 2026 hckaye。

DryDBは別プロジェクトで、こちらもMITライセンス、Copyright (c) 2024 hadashiA。そのソースはこのリポジトリに含めても再配布してもいない。`tests/interop/fetch-upstream.sh` が固定commitをgit管理外のディレクトリにcloneし、相互運用テストの実行にだけ使う。形式の実装にあたって参照したソースと文書は、互換性仕様の[一次資料](docs/compatibility.md#8-一次資料)にcommit固定のリンクで示してある。
