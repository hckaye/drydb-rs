# ベンチマーク

## 実行方法

```sh
cargo bench -p drydb --features zstd
```

規模は環境変数で変えられる。既定より小さくすると試行が速くなり、大きくするとcacheに入らない領域の測定が安定する。

| 変数 | 既定 | 対象 |
| --- | --- | --- |
| `DRYDB_BENCH_ROWS` | 200000 | 読み取りベンチの行数 |
| `DRYDB_BENCH_PROBES` | 100000 | 点検索の回数 |
| `DRYDB_BENCH_BUILD_ROWS` | 200000 | builderベンチの行数 |
| `DRYDB_BENCH_VALUE_LEN` | 32 | builderベンチの値の長さ |

debug buildで動かすと、測っているのはcheckの費用になる。ベンチ本体がその旨を出力する。

## 測っているもの

`benches/queries.rs` は読み取り経路を測る。作業集合は2通りあり、ファイル全体がcacheに収まる場合と、cacheがファイルの16分の1しかない場合を両方出す。片方だけを載せると、多くの利用者が置かれていない状況を説明することになる。

軸は次のとおり。

- page size 256 / 4096、digestのsorted / Eytzinger
- キーは `i64`（キーのbyte列を持たないlayout）と、先頭8 bytesが共通する `ascii`（digestが衝突してキー比較に落ちるlayout）
- 点検索、全件走査の昇順と降順、100行の範囲、全件count、1キーあたり64行の副索引
- 報告はthroughput、1操作あたりのp50 / p95 / p99、allocation回数、cache hit率

`benches/build.rs` はbuilderを測る。入力は毎回shuffleしてあるので、sorterは実際に並べ替えをする。sort bufferを変えて、全部がメモリに載る場合と、一時ファイルへ書き出してmergeする場合を比べる。

点検索のベンチは結果の値を読んでchecksumに混ぜる。値を読まないと、最適化で検索ごと消える可能性がある。乱数列は測定の外で作る。

## 基準値

以下は Apple M系 aarch64 / macOS、rustc 1.98.1、release profile、200000行、32 bytesの値での実測。マシンとtoolchainが違えば数値も変わる。比較に使えるのは同じ環境で取り直した数値だけになる。

既定の設定で測っている。digestとキーの照合（`OpenOptions::validate_digests`）は有効。

同じ表を数回取り直すと、行によっては2割ほど振れる。以下は1回の実行から採った値で、行ごとに別の実行を混ぜてはいない。

### 点検索

| 構成 | 作業集合 | throughput | p50 | p99 | cache hit率 |
| --- | --- | --- | --- | --- | --- |
| page 4096 | cacheに収まる | 2.31 M op/s | 375 ns | 1.38 µs | 98.6% |
| page 4096 | cacheは16分の1 | 0.63 M op/s | 1.33 µs | 3.50 µs | 45.4% |
| page 4096, Eytzinger | cacheに収まる | 2.06 M op/s | 375 ns | 2.75 µs | 98.6% |
| page 4096, Eytzinger | cacheは16分の1 | 0.34 M op/s | 2.58 µs | 6.04 µs | 44.5% |
| page 256 | cacheに収まる | 1.24 M op/s | 625 ns | 1.96 µs | 92.1% |
| page 256 | cacheは16分の1 | 0.72 M op/s | 1.38 µs | 2.08 µs | 64.0% |

先頭8 bytesが共通する `ascii` キーでの点検索は 0.78 M op/s、p50 1.25 µs。digestが衝突して、葉の中でキー比較に落ちる分が乗る。

ページ1枚あたりの管理費の見積り（`BUFFER_OVERHEAD`）を128 bytesから192 bytesへ上げてある。同じcache容量に入るページ数がその分減るので、cacheがファイルの16分の1の構成ではhit率が下がる。実測で、page 256は64.0%から62.5%、page 4096のEytzingerは44.5%から44.4%になった。hit率は比なのでマシンの負荷に影響されない。throughputもその分下がるが、上の表はこの変更より前に、他の負荷が無い状態で取った値になっている。

その後、cacheのshardごとの表とringを、定数ではなく実際の確保量で計上するように変えた。既定のshard数ではページに使える分がほとんど変わらないので、hit率は取り直しても同じで、page 4096は98.6%と45.4%、Eytzingerは98.6%と44.4%、page 256は92.1%と62.5%になる。

Eytzingerの行だけcacheが16分の1のときに遅いのは、digestとキーの照合が効いているため。この照合はページを読み込んだ木が最初にそのページへ触れたときに1回走り、1 entryにつきキーを1件取り出してdigestを計算する。`i64` のsorted構成ではキーのbyte列を持たないlayoutが選ばれるので、突き合わせる相手が無く照合自体が走らない。Eytzingerはこのlayoutと併用できないため、キーが格納され、照合が毎回走る。

照合を止めて同じ測定を取ると、Eytzingerでcacheが16分の1の構成は 0.65 M op/s、p50 1.38 µsになる。この構成では読み込みが多く、1ページあたりのentry数が80件前後あるので、照合の費用がそのまま乗る。cacheに収まる構成では読み込みがほとんど起きないので差は小さい。`OpenOptions` の既定を変えるつもりはない。照合を切ると、digestとキーが食い違うファイルは「エラー」ではなく「行が足りない答え」を返すようになる。

### 走査とcount

| 操作 | cacheに収まる | cacheは16分の1 |
| --- | --- | --- |
| 全件走査（昇順） | 27.7 M row/s | 21.6 M row/s |
| 全件走査（降順） | 27.6 M row/s | 21.4 M row/s |
| 100行の範囲 | p50 4.25 µs | p50 6.46 µs |
| 全件count | 987 M row/s | 100 M row/s |

countが走査より2桁速いのは、値を読まず、範囲に完全に入る葉ではentry countを足すだけだから。

### allocation

暖まった状態での点検索、走査の1ステップ、countは、いずれも1回もallocateしない。`crates/drydb/tests/allocations.rs` がallocatorを差し替えてこれを検査する。cold readはpage bufferを確保するので対象外で、確保量がページ1枚分に収まることだけを見る。

### build

| 構成 | throughput | 出力 |
| --- | --- | --- |
| page 4096、sortがメモリに収まる | 1.88 M row/s | 43.0 bytes/row |
| page 4096、sort buffer 1 MiB（一時ファイルへ書き出す） | 0.39 M row/s | 43.0 bytes/row |
| page 4096、Eytzinger digest | 2.31 M row/s | 59.7 bytes/row |
| page 4096、副索引1つ | 1.33 M row/s | 84.7 bytes/row |
| page 4096、zstd | 2.28 M row/s | 6.4 bytes/row |
| page 65536、classic metadata | 3.51 M row/s | 56.1 bytes/row |

Eytzinger digestは出力が4割ほど大きくなる。digest配列を2のべき乗に埋めるのに加えて、キーのbyte列を省略するlayoutが使えなくなるため。

## 測定して採用した最適化

### 木ごとにrootページを保持する

すべての検索がrootページから始まるので、木ごとに1枚だけ保持してcacheの参照を1回減らす。ページ1枚分を予算へ計上し、予算に余裕が無いときは保持しない。保持するのはpage store側で、予算が足りなくなったときはcacheとpage directoryを空にしたあとの最後の手段として解放する。

50000行、page 4096、cacheに収まる構成で、430 ns/opから365 ns/opになった。cacheが16分の1の構成では3.77 µsから2.92 µsになった。

### 副索引cursorのキーコピーを1回にする

行ごとに索引キーを一時 `Vec` へ取り出してからcursorのbufferへ移していたのを、直接bufferへ書くようにした。1キーあたり64行の副索引を5000回引く測定で、allocation回数が996933回から20365回になった。残っているのは問い合わせごとの境界キーの確保で、行ごとではない。

## 採用していないもの

次の項目は形式を変えずに試せるが、採用していない。

| 候補 | 状況 |
| --- | --- |
| SIMDによるdigest検索 | scalar実装との一致をproperty testで示し、CPU検出とfallbackを入れる作業が要る |
| cacheのlock削減 | 安全なreclamationの設計と並行モデル試験が先に要る |
| page directoryのdense化 | 小規模なファイルに限ってメモリと引き換えになる。総メモリの増分を測っていない |
| prefetch | ランダムreadでのI/O増加、cache汚染、p99への影響を測っていない |
| mmapの既定化 | 別の安全性契約になる。featureとしては使えるが、既定にはしない |

## upstreamとの比較について

upstreamのREADMEにある数値は、この実装の性能根拠には使わない。同じマシン、同じデータ、同じ問い合わせ列で測り直さない限り、2つの実装の数値を並べても比較にならない。相互運用テストのharnessは両実装を同じ入力で動かせるので、比較したい場合はそこから始められる。
