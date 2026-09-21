# タスクリスト

状態: すべて実装済み。チェックは、記載した成果物とテストが存在し、通っていることを指す。

対応するテストの場所は各項目に書いてある。相互運用テスト（`crates/drydb-interop`）は、`tests/interop/fetch-upstream.sh` を実行したうえで `DRYDB_INTEROP=1 cargo test -p drydb-interop` で走る。

## M0: 基準と基盤

- [x] **C01: upstreamを固定してwire仕様を確定する。** Header、descriptor、PageRef、node metadata、flags、sentinel、filterの配置、duplicate keyの符号化をoffsetと幅つきで[互換性仕様](compatibility.md#3-ソースで確認した形式)に記録した。
- [x] **C02: C# fixture generator / reader oracleを作る。** `tests/interop/DryDbOracle` が固定commitのupstreamを参照する。Rustの通常buildに.NETは要らない。
- [x] **C03: query境界と互換例外を確定する。** 空・無限端・排他端・prefix・i64極値・重複副索引を実測し、合わなかった9件を[例外台帳](compatibility.md#6-例外台帳)に記録した。各項目にupstreamの実測値を固定したテストがある。
- [x] **C04: fixture matrixを揃える。** 12件のfixtureが全node layout、複数table、副索引、overflow、長さ境界、zstdを覆う。破損入力は `crates/drydb/tests/corrupt.rs` が扱う。
- [x] **F01: Rust workspaceとCI方針を定める。** MSRVは1.85で、library、全feature、テスト一式が1.85でbuildできることを確認した。CI設定は `.github/workflows/ci.yml`。
- [x] **F02: 共通の予算・診断・ベンチ基盤を作る。** `Budget`、`MetricsSnapshot`、`MemoryReport` と、allocation回数を数えるベンチ用harness。
- [x] **R01: rkyvと所有権を確認する。** rkyv 0.8のprofile検出、任意offsetの配置、alignment、値単位の再配置、safe accessの借用を `crates/drydb-rkyv` で扱う。

## M1: 無圧縮reader

- [x] **D01: safe format parserを実装する。** `crates/drydb/src/format`。全layoutをchecked decodeで処理し、破損をpanicなしに拒否する。
- [x] **D02: positional PageSourceを実装する。** `crates/drydb/src/io.rs`。短いread、EOF、`EINTR` を扱い、共有Seek位置を持たない。
- [x] **D03: bounded PageDirectoryを実装する。** `crates/drydb/src/directory.rs`。必要chunkだけを読み、ページ数に比例する常駐配列を作らない。
- [x] **D04: PageBuffer / PagePin / ValueGuardを実装する。** `crates/drydb/src/page.rs`。guard保持中の退避とdatabase dropでもsliceが有効。
- [x] **D05: bounded cacheとsingle-flightを実装する。** `crates/drydb/src/cache.rs`。同時miss、失敗、load中のpanic、退避後のguard保持を `tests/concurrency.rs` で確認する。
- [x] **D06: B+Treeのpoint lookupを実装する。** `crates/drydb/src/btree.rs`。i64 / ascii / uuidv7 / ulid、全layout、digest衝突、hitとmissがC# fixtureと一致する。
- [x] **D07: メモリ上限の不変条件を検証する。** cache予算の16倍以上のdatabaseでも全件をロードせず検索でき、guard保持による不足は待機ではなくエラーになる。`tests/concurrency.rs` と `tests/roundtrip.rs`。

## M2: query・副索引・BLOB

- [x] **Q01: borrowed cursorを実装する。** `crates/drydb/src/query.rs`。seek、advance、current、昇降順、葉間移動、キーの復元を扱い、結果を保持しない。
- [x] **Q02: range / prefix / countを実装する。** 境界の全組合せをC#と比較した。countは値もcodecも読まず、allocateもしない。
- [x] **Q03: unique / 非unique副索引を実装する。** `crates/drydb/src/index.rs`。PageRefの解決、重複キーの規則、同値内の順序、排他境界。upstreamと合わない2点は例外台帳のD1とD2。
- [x] **Q04: overflowと無圧縮BLOB streamingを実装する。** `crates/drydb/src/blob.rs`。inlineとoverflowの切替、予算に入らない大きさの値、壊れた参照を扱う。
- [x] **Q05: reader意味論の差分テストを自動化する。** 相互運用テストが境界の組合せを網羅した問い合わせ列を双方で実行する。`crates/drydb/tests/property.rs` は `BTreeMap` と比較する。

## M3: builderと双方向互換

- [x] **B01: sorted streaming builderを実装する。** `crates/drydb/src/builder/tree.rs`。葉と内部node、rootのpatch、directoryを生成し、空入力、重複、サイズ上限、失敗を扱う。
- [x] **B02: spoolとexternal sortを実装する。** `crates/drydb/src/builder/spool.rs`。sort bufferを超えたら一時ファイルへ書き出し、k-way mergeで読み戻す。同じキーの順序はappend順で安定する。
- [x] **B03: 副索引・overflow writerを完成させる。** 全layout、複数table、重複副索引、巨大値をC# readerから参照できる。
- [x] **B04: 無圧縮の双方向interopをCI化する。** 12件のfixtureで双方向を確認する。10件は両実装の出力がbyte単位で一致する。
- [x] **B05: 一時ファイルと公開処理をhardeningする。** 一時ファイルへ書き、`sync_all` のあとrenameで公開する。失敗時はdropで削除し、既存のファイルに触れない。

## M4: 互換範囲の拡張

- [x] **X01: 標準encodingと拡張registryを揃える。** i64 / ascii / uuidv7 / ulidを実装し、`Guid` と `Ulid` の順序は.NET上で実測して確定した。未知のIDは拒否する。
- [x] **X02: 標準page filterを移植する。** `DryDB.ZstdCompression` を双方向で確認した。復号後のサイズはfilterの申告を使い、上限と予算で抑える。2つ以上のfilterは例外台帳のD3。
- [x] **X03: MessagePack adapterを追加する。** `crates/drydb-msgpack`。配列形式とmap形式のfixture DTOで双方向を確認した。任意DTOの互換は宣言しない。
- [x] **X04: async adapterを実装する。** `crates/drydb-async`。cacheに載っていれば同期で答え、そうでなければblocking poolへ移す。futureのdropでも資源を取り残さない。
- [x] **X05: CLIを追加する。** `crates/drydb-cli`。inspect、verify、get、range、prefix、count、build。verifyは1ページずつ検査する。
- [x] **X06: 拡張後の互換性マトリクスを公開する。** [対応範囲](compatibility.md#5-対応範囲)。対象外（custom拡張、Unity、旧format）も書いてある。

## M5: rkyv統合

- [x] **R02: envelope / schema / profile仕様を確定する。** 24 bytesのenvelopeをgolden bytesのテストで固定した。rkyvのfeature構成は起動時に検出し、envelopeに記録して読み出し時に照合する。
- [x] **R03: レコード単位serializerを実装する。** 1値ごとに独立したarchiveを作る。serializerの状態はレコード間で持ち越さない。
- [x] **R04: alignment対応の借用readerを実装する。** 整列していれば借用、していなければその1値だけをcopyする。copy禁止modeでは明示的に失敗する。
- [x] **R05: 型・検証・寿命をhardeningする。** schemaとprofileの不一致、切り詰め、壊れたarchive、別値へのpointer、guard寿命、退避、予算不足を扱う。`crates/drydb-rkyv/tests/codec.rs`。
- [x] **R06: rkyvの統合fixtureと計測を揃える。** databaseへ格納した値の往復、raw bytesが保存されること、borrowとcopyの割合を測る。

## M6: 測定・最適化・リリース

- [x] **P01: 再現可能なbaselineを保存する。** [ベンチマーク](benchmarks.md)。cacheに収まる場合と16分の1の場合、allocation、p99、cache hit率、読み取りbyte数。
- [x] **P02: 安全なhot path最適化を評価する。** rootページの保持と副索引cursorのキーコピー削減を採用し、前後の数値を記録した。
- [x] **P03: SIMD / branchless経路を評価する。** 採用しない。理由は[ベンチマーク](benchmarks.md#採用していないもの)。
- [x] **P04: atomic / lock削減を評価する。** 採用しない。同上。
- [x] **P05: mmap / dense directory / prefetchを比較する。** mmapはoptional featureとして提供し、既定にはしない。dense directoryとprefetchは採用しない。同上。
- [x] **P06: rkyv追加最適化を評価する。** unaligned profileとvalue paddingは採用しない。profileを変えると、同じ型定義で読めるファイルが変わる。
- [x] **H01: property / fuzz / Miriを用意する。** `tests/property.rs`、`fuzz/` の4 target、`tests/miri_smoke.rs`。Miriはlibrary内のunit test全件とsmoke testで通る。`drydb-rkyv` のテストは、依存しているbytecheckのslice検査がStacked Borrowsで未定義動作として報告されるため、Tree Borrows（`-Zmiri-tree-borrows`）で実行する。そちらでは全件通る。
- [x] **H02: 並行性・資源枯渇試験を用意する。** `tests/concurrency.rs`。single-flight、退避中のread、予算の枯渇と回復、guardを持ったままのdatabase drop。
- [x] **H03: リリース文書とfeature / target CIを用意する。** 対応範囲、MSRV、メモリ契約、unsafe一覧、ライセンス、利用例、ベンチ手順。CI設定は `.github/workflows/ci.yml`。
