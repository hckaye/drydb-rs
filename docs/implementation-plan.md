# 実装計画と到達状況

状態: M0からM6まで実装済み。最終更新: 2026-09-20。

## 1. マイルストーンの到達状況

| 段階 | 成果物 | 状況 |
| --- | --- | --- |
| M0 | workspace、upstream fixture harness、wire仕様、rkyvと所有権の確認 | 完了。upstreamは固定commitから取得し、wire仕様は[互換性仕様](compatibility.md)にある |
| M1 | safe parser、bounded directory / page cache、無圧縮のpoint reader | 完了。全layoutのC# fixtureを読み、予算超過と不正入力を安全に拒否する |
| M2 | range / prefix / count / cursor、副索引、overflow | 完了。C#との結果比較が通り、scanは結果を保持しない |
| M3 | streaming builder、external sort、directory spool | 完了。双方向の相互運用テストが通る |
| M4 | 標準encoding / filter、MessagePack adapter、async、CLI | 完了。対応表の各項目にfixtureが対応する |
| M5 | rkyv value codec、schema / profile、整列fallback、型付き参照 | 完了。alignment、破損、寿命、メモリの試験とraw bytes相互運用が通る |
| M6 | 測定、hardening、利用例 | 完了。ベンチマーク、fuzz、Miri、並行性試験があり、採用した最適化には前後の数値がある |

## 2. 検証の構成

### 相互運用

C# oracleはupstreamの固定commitを別プロセスで動かす。`tests/interop/fetch-upstream.sh` がcommitを取得し、`tests/interop/DryDbOracle` がそれを参照するconsole applicationになる。Rust利用者の通常のbuildに.NETは要らない。

1件のfixture specから、C#とRustの双方がdatabaseを作り、双方が同じ問い合わせ列を実行する。4通りの組合せを比較するので、「相手のファイルを読める」ことと「相手がこちらのファイルを読める」ことと「問い合わせの意味が一致する」ことを分けて確認できる。

fixtureは12件あり、i64 / ascii / uuidv7 / ulidのキー、classicとcompactのmetadata、Eytzinger digest、OmittedKeys、overflow値、unique / 非uniqueの副索引、zstd圧縮を覆う。実行結果と、両実装が生成したファイルのSHA-256は `tests/interop/work/manifest.txt` に出る。

upstreamの挙動に合わせなかった項目は、[互換性仕様の例外台帳](compatibility.md#6-例外台帳)に記録し、それぞれupstream側の実測値をassertionとして固定したテストがある。

### 安全性

| 対象 | 方法 |
| --- | --- |
| 破損入力 | 有効なファイルの1 byte書き換えと先頭切り詰めを多数作り、全読み取り経路を実行する |
| 未知の入力 | 乱数byte列、未知のencoding ID、未知のfilter ID、filter 2つ以上の宣言 |
| 借用と寿命 | Miriで、library内のunit test全件と、builderからcursorまでを一周するsmoke testを実行する |
| 並行性 | 同時missの集約、退避中のread、予算の枯渇と回復、guardを持ったままのdatabase drop |
| メモリ | allocatorを差し替えて、暖まった点検索・scan・countのallocation数を数える |
| 検索の意味論 | `BTreeMap` を模範解答として、page sizeとdigest layoutを変えながらproperty testで比較する |

`fuzz/` にcargo-fuzz用のtargetがある。手順は[ベンチマーク](benchmarks.md)と同じく、CIの必須jobには入れず、手元とnightly jobで回す。

## 3. 性能評価

「RustはC#より速い」という仮定は置かない。測定の構成と、この実装で採用した最適化の前後の数値は[ベンチマーク](benchmarks.md)にある。

ベンチマークは、cacheに収まる作業集合と、cacheの16倍の作業集合の両方を測る。片方だけを載せると、多くの利用者が置かれていない状況を説明することになる。報告する項目はthroughput、1操作あたりのp50 / p95 / p99、allocation回数、cache hit率、読み取りbyte数、予算の使用量。

upstreamのREADMEにある固定の目標値は使わない。

## 4. リリースの前提

対応targetは64-bitのLinux / macOS / Windows、x86_64とaarch64を想定する。MSRVは1.85で、libraryと全featureとテスト一式が1.85でbuildできることを確認してある。32-bit、big-endian、Wasmは検証していないので対応を宣言しない。

公開時に添えるものは、[互換性マトリクス](compatibility.md#5-対応範囲)、[例外台帳](compatibility.md#6-例外台帳)、メモリ契約の説明、unsafeの一覧（`mmap` featureの1か所のみ）、ライセンス表示、再現可能なベンチマーク手順。

## 5. 設計判断の記録

| 判断 | 選んだ理由 | 棄却した案 |
| --- | --- | --- |
| 形式の復号はすべて明示的なlittle-endian読み取り | 不正な入力でUBに到達しないことを型と検査で保証できる | upstreamと同じ構造体castは、範囲検査が無く安全性を移植できない |
| cacheはsharded mutexとCLOCK | 正しさを先に確定させ、測定してから最適化する | lock-freeは、reclamationの設計と並行モデル試験が別途必要になる |
| 復号後のページ長はfilterの申告を使う | 上限をそのまま予約すると、既定の予算では圧縮ファイルが開けなくなる | 常に上限を予約する案は、page sizeと無関係に大きな予約が必要になる |
| page filterは1つまで | upstreamが2つ以上を書くことも読むこともできない | 合成順序を推測すると、どのファイルとも一致しない形式を作ることになる |
| 空のtableにも空のleafを書く | C# readerから読める。upstreamの`-1`は自分のreaderで例外になる | upstreamと同じ`-1`を書くと、生成したファイルがC#から開けない |
| 非ユニーク副索引はキーのbyte列を必ず書く | 同じ索引キーを持つ行をページ上で区別できる | upstreamと同じ省略は、record idがdigestに入らないため行を区別できない |
| prefixはbyte順のencodingに限る | byte prefixが範囲を表さないencodingで、黙って違う結果を返さない | encodingを問わず受け付けると、`i64`で無意味な結果になる |
| rkyvのalignment要求は16 bytesを既定にする | rootの`align_of`では、archive内部の64-bit値の整列を保証できない | rootのalignmentだけを見る案は、実際に検証で失敗した |
