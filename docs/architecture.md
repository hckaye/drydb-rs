# アーキテクチャ

状態: Proposed。以下の型名・モジュール名・設定名は設計案であり、実装済みAPIではない。

## 1. 目的と非目的

DryDB 1.4互換の不変ファイルを、必要なページだけ読みながら高速に検索する。シード時に書込みを完了し、通常のDBハンドルはreadonlyとする。互換性、安全性、管理メモリ上限を守った上で、読み取りコストを最小化する。

全件RAM常駐、全件HashMap化、DB全体のrkyv archive化、全ページの起動時検証は既定動作にしない。SQL、更新API、WAL、MVCC、オンラインのindex追加、汎用トランザクション、DBサーバープロトコルは作らない。

## 2. コンポーネント

```text
Seed input -> Builder / external sort -> DryDB 1.4 file
                  | value codec                  |
                  +-- raw / MessagePack / rkyv    |
                                                 v
Application -> Database / Table / Index / Cursor
                         |
                     B+Tree search
                         |
                 PageStore + PagePin
                  /              \
       bounded page cache      PageDirectory
                |            (bounded chunk cache)
          PageSource + filter pipeline
                |
          positional file I/O

Optional backend: read-only mmap (別のメモリ・安全性契約)
Typed values: raw ValueGuard -> optional codec -> archived / owned value
```

想定workspaceは以下。最初から細かいcrateへ分割しすぎず、core内部はmodule境界で分離する。

| crate / 領域 | 責務 |
| --- | --- |
| `crates/drydb` | `format`, `catalog`, `directory`, `io`, `cache`, `btree`, `query`, `builder`, `error`, `metrics` |
| `crates/drydb-rkyv` | レコード単位のrkyv codec、payload profile、検証、整列buffer。coreにrkyv依存を持ち込まない |
| `crates/drydb-cli` | 将来のbuild / inspect / verify / query。型付きシード生成は利用側プログラムでも可能 |
| 任意のMessagePack adapter | DTO別のcodec。coreのraw bytes APIとは独立 |
| `tests/interop` | 固定upstreamのC# oracle、fixture生成・消費、相互運用の比較 |
| `benches` | 同一fixture・同一workloadを使うCPU、I/O、メモリ、serializer計測 |

## 3. 形式層と検証境界

wire formatは[互換性仕様](compatibility.md)に従う。diskのbyte列を`repr(C)` structへ直接castしてRust参照にしない。まず境界を確認し、`from_le_bytes`等の明示的なdecodeで数値を得る。

`PageOrdinal`, `FileOffset`, `PageLocalOffset`を別型にする。ファイル側のsigned fieldは負値・sentinelを判定してから内部型へ変換する。加算・乗算・usize変換はchecked演算とする。

openではboundedなheader/catalogを読み、version、名前長、個数、directory領域の範囲を確認する。rootとデータページは原則lazy。ページ読取時にnode kind、flags、offset列、digest領域、子参照、PageRef、payload範囲を確認する。未知のflagは黙って受理しない。

不正な木の循環や極端な深さは、深さ・走査step・作業量の上限で停止させる。全件検証は明示的な`verify`操作としてストリーミング実行し、openの隠れた全件走査にはしない。

## 4. I/Oとページディレクトリ

### 既定: positional I/O + bounded cache

共有`Seek`位置を使う`Mutex<File>`を検索経路の中心に置かない。`PageSource`は位置指定readを抽象化し、短いread、割込み、EOFを扱う。プラットフォームごとの差はadapter内に閉じ込める。

`PageDirectory`はordinalから8-byte offsetを取得する。既定は固定サイズのdirectory chunkを必要時に読み、bounded cacheへ保持する。全offset配列やpage count分の空cache slotを自動確保しない。dense配列経路は予算確認付きの明示的な最適化候補に留める。

実ページのlengthを検査し、予算を予約してからbufferを確保する。filterはロード境界で適用し、解凍済みページを不変bufferとして公開する。圧縮前サイズ・解凍後サイズ・同時scratchを別々に制限し、未知のfilterやdecompression bombを拒否する。

### 任意: mmap

mmapは全件RAMロードではないが、OSのfaultやresident setをページキャッシュ予算だけで制御できるわけでもない。従って既定backendにせず、別契約として選択可能にする。prefault / populateは既定で行わない。

`memmap2`のfile-backed mappingは外部からの変更等による安全性条件がある。read-onlyで開くことだけでは、他プロセスの変更・truncateを防げない。[M1]

ファイル不変性をプラットフォーム上で保証できるsafe wrapperを用意するか、明示的なunsafeなopen契約に分離する。単なる「シード後は変更しない」という文書上の約束を根拠にsafe APIの内部で無条件にmapしない。COW mapだけで問題が消えるとも扱わない。圧縮ページはmmap上のbytesからそのまま型付き参照できない。

## 5. ページ所有権と読み取りAPI

cacheが所有する不変`PageBuffer`と、利用中の生存期間を保証する`PagePin`を分ける。ここでのpinはページ退避との関係を表す概念であり、`std::pin::Pin`を使えば自動的に実現するものではない。

`ValueGuard`は`PagePin`と検証済みの値範囲を所有し、`as_bytes(&self) -> &[u8]`を返す。借用したsliceはguardより長生きできない。cacheから追い出されてもguardがある間はbufferを再利用・解放しない。Database本体のdropと、残存Table / Cursor / ValueGuardの資源寿命を分離する。

APIの形は以下を想定する（署名の概略）。

```text
Database::open(path, OpenOptions) -> Result<Database>
Database::table(name) -> Result<Table>
Table::get(key) -> Result<Option<ValueGuard>>
Table::range(Bound<Key>, Bound<Key>, Order) -> Result<Cursor>
Table::prefix(prefix) -> Result<Cursor>
Table::count_range(bounds) -> Result<u64>
Table::index(name) -> Result<Index>
Cursor::advance(&mut self) -> Result<bool>
Cursor::current(&self) -> Option<EntryRef<'_>>
ValueGuard::as_bytes(&self) -> &[u8]
```

通常のscanは現在ページだけを保持し、結果全件を`Vec`へ集めない。`EntryRef`はcursorから借用するので、その参照を利用中には次ページへ進めない。保持したい値は明示的にowning guard化またはcopyする。省略キーの復元にはcursor内の小さなbufferを使い、その借用寿命も同じ規則に従わせる。

副索引cursorは索引ページと参照先ページを必要時だけ保持する。countでは値の取得・デコードをしない。完全に範囲内の葉ではentry countを利用し、境界だけを検索する。互換formatに存在しないsubtree countを勝手に追加してO(log N)を保証しない。

## 6. キャッシュと並行性

初期実装はsharded map + `Arc<PageBuffer>`等の安全な所有権を使い、短いlock区間内で生存する所有権を取得する。S3-FIFO / CLOCK系の退避方針はポリシーとして分離する。I/O、解凍、ユーザーcallback、値の参照中にcache lockを保持しない。

同一ページの同時missはsingle-flightでまとめる。状態は概ね`Vacant -> Loading -> Ready`とし、失敗・キャンセル時には予約と待機者を必ず解放する。cache keyにはDBの世代とordinalを含め、ファイル差替え後のページを混在させない。

upstreamのGC前提のoptimistic retainをRustのraw pointerに翻訳しない。raw pointerをloadしてから無保護で`Arc::increment_strong_count`することも禁止する。lock-free化はepoch / hazard等のreclamation設計と並行テストを含む独立した最適化PRで検討する。

readonlyでもcacheの内部可変性と参照保持の同期は必要になる。「DB全体にRwLockを掛けない」と「一切のatomic / lockが不要」は同義ではない。rootや現在leafの借用再利用でrefcount更新を減らし、同一hot key競合と分散keyを分けて測る。

## 7. メモリ契約

`OpenOptions`に総管理予算と内訳上限を設ける。具体的な初期値はM0の計測で確定する。少なくとも次を計上する。

```text
charged memory = unique live page buffer capacities
               + directory / catalog / cache metadata
               + in-flight I/O and decompression buffers
               + rkyv alignment / validation scratch
               + optional acceleration data and bounded pools
```

cacheとguardが同じbufferを共有する場合、buffer容量は一度だけ計上する。退避後もguardで保持されるbufferは予算を消費し続ける。thread-localのhot pageやbuffer poolも例外にしない。page数だけでは巨大overflow pageを管理できないため、byte容量で制御する。

全allocationを無条件に厳密なRSS上限へ変換できるとは約束しない。管理するallocationのcapacity、metadata、予約量を追跡し、allocator自身の内部費用、利用側が別途作った値、OS page cache、mmap residencyとは区別してメトリクスを出す。

新規readでは必要な最大buffer容量を先に予約し、予算内で退避できなければ既定で`BudgetExceeded`を返す。利用側がguardを保持したままreadするケースを考慮し、暗黙に無限待機してdeadlockしない。再試行可能か、要求量・使用量がいくらかをエラーに含める。

最低限のtree traversalにも複数bufferが必要になる。設定時の最低予算チェックに加え、巨大ページ、深い木、多数の同時readについて実行時の予約失敗を扱う。予算を超えるrkyv値を黙って全展開しない。

## 8. BLOBと巨大値

DryDBのoverflow格納をそのまま読み書きする。通常の値はページからsliceを返すが、巨大値のページ確保は別途予算検査する。

無圧縮のraw BLOBには位置指定のchunk readerを設け、巨大値を丸ごと保持せず転送可能にする。圧縮BLOBのstreamingはfilterのframingとdecoder能力を確認した後の機能とし、未対応時はサイズ上限付きのmaterializeまたは明示的エラーとする。

rkyvの型付き参照は対象archiveの連続した領域と検証が必要になるため、raw BLOBのstreamingと同じ保証をしない。大きすぎるオブジェクトはアプリケーション側で複数レコードに分割する。

## 9. シード・builder

builderとreadonly readerの型を分ける。初期builderはソート済み入力を逐次受け取り、葉・内部nodeをbottom-upで生成する。未ソート入力はメモリ予算付きexternal sortで扱い、小規模向けの全件bufferingは明示的な補助APIにする。

副索引用のkey / PageRef、未確定root位置、directory offsetを一括`Vec`へ蓄積する設計は避ける。必要な中間情報は一時ファイルへspoolし、sorted merge・後方patch・directory追記で仕上げる。seed自体がDB全量をRAMへ渡すことを必須にしない。

一意制約、入力順、長さ、ordinal上限を検査する。rkyvは1レコードごとにserializeし、workerごとのscratchと並列数を制限する。圧縮もpage境界で実施する。

出力は一時ファイルに構築して検証後に公開する。flush / sync / rename等の保証はOSごとに明文化し、実行時transactionとは呼ばない。開かれているファイルをin-place更新しない。途中失敗・キャンセル時の一時ファイルcleanupも実装対象とする。

## 10. Rust側の最適化候補

| 候補 | 前提・評価方法 |
| --- | --- |
| enum / genericによるencoding特殊化 | i64 / asciiのhot pathで動的dispatchを削減。custom encoding経路は残す |
| digest列上の検索 | upstreamのsorted / Eytzinger形式を維持。衝突fallback・padding・端点の正しさが先 |
| borrowed cursor / page再利用 | per-row allocationとrefcountを削減。保持ページは予算へ計上 |
| root / upper-page保持 | bounded・lazy。全tableの全rootを無条件preloadしない |
| bounded prefetch | 連続scanで候補。ランダムreadのI/O増加、cache汚染、p99を確認 |
| SIMD / branchless / unaligned数値load | portable scalar版と比較し、CPU feature detectionとfallbackを残す |
| cache metadataのdense化 | 小規模DBなど予算に収まる明示的modeのみ。初期化と総メモリ費用も比較 |
| lock / atomic削減 | reclamationの安全性を証明し、Miri・並行モデルテスト・競合benchを通す |

これらは速度向上の仮説である。Rustであることやunsafeを使うことだけを性能根拠にしない。ファイル形式を変える最適化は別のformat proposalとし、1.4互換writerに混ぜない。

## 11. 非同期とエラー

sync coreを先に完成させる。async adapterはruntime依存をoptionalにし、blocking I/Oをそのままasync関数に入れない。blocking pool方式とnative async方式のどちらかは実測と対応OSで決める。途中キャンセルでもPagePin、予算予約、single-flight待機者を漏らさない。

代表的なエラー区分は`Io`, `UnsupportedFormat`, `UnknownEncoding`, `UnknownFilter`, `CorruptData`, `BudgetExceeded`, `ValueTooLarge`, `SchemaMismatch`, `ArchiveValidation`。不存在は`Option`で区別する。エラーにはページ番号・offset等の診断情報を含めるが、値の機密データを無条件にログ出力しない。

## 12. 安全性の完了条件

不正なファイルを入力してもsafe APIからUBに到達させない。parserと通常のI/O経路はsafe Rustを基本とし、unsafe箇所にはalignment・bounds・初期化・aliasing・寿命・reclamationの不変条件を記載する。

eviction中のread、guard保持中のDB drop、同一pageへの同時miss、キャンセル、rkyvの壊れたrelative pointerを必須テストにする。追加のunsafe高速化は、同じ入力に対するsafe reference実装との一致を要求する。

## 参考

- upstreamの形式・キャッシュに関する根拠: [互換性仕様の一次資料](compatibility.md)
- rkyvの参照・検証・alignment: [rkyv設計](rkyv-design.md)
- [M1: memmap2 MmapOptions / Safety](https://docs.rs/memmap2/latest/memmap2/struct.MmapOptions.html)（調査時0.9.11、2026-09-20）
