# 実装計画

状態: Proposed / 2026-09-20。期間の見積りではなく、依存関係と完了条件に基づく順序を示す。現時点では設計文書のみで、以下のマイルストーンは未達成。

## 1. 実装順序

```text
M0 互換性の基準・安全性spike
  -> M1 無圧縮reader
     -> M2 query / index / BLOB
        -> M3 streaming builderと双方向相互運用
           -> M4 標準プラグイン・async・CLI
              -> M5 rkyv統合
                 -> M6 計測に基づく最適化・リリース準備
```

rkyvのalignment / profile調査（R01）とベンチマーク基盤はM0から並行して進める。rkyvの最小統合はM1〜M3で進められるが、M5の完了判定には圧縮・BLOB・副索引経由も含める。cacheの安全性を後回しにしてunsafe版を先に実装する順序にはしない。

## 2. マイルストーン

| 段階 | 成果物 | 完了条件 |
| --- | --- | --- |
| M0 | workspace方針、upstream fixture harness、wire仕様、rkyv / 所有権spike | 固定C#版でfixtureを再生成でき、未確定事項が台帳化される |
| M1 | safe parser、bounded directory / page cache、無圧縮point reader | 全有効node layoutのC# fixtureを読み、予算超過と不正入力を安全に拒否 |
| M2 | range / prefix / count / cursor、副索引、overflow | C# oracleとの結果・順序・境界比較が通り、scanで結果全件を保持しない |
| M3 | sorted streaming builder、external sort、directory / index spool | C#→Rust、Rust→C#の無圧縮コア互換。大きなseedでも指定予算内 |
| M4 | 標準encoding / filter、MessagePack adapter、async、inspect / verify | 公開対応表の各項目がfixtureで裏付けられ、未知の拡張は明示的エラー |
| M5 | rkyv value codec、schema / profile、整列fallback、型付き参照 | alignment / corruption / lifetime / memory試験とraw bytes相互運用が通る |
| M6 | CPU / I/O / メモリ最適化、hardening、利用例 | safe基準実装との一致、再現可能bench、feature / platform別対応表を公開 |

## 3. PR分割方針

今回のPRは文書のみとする。以降は`docs/tasks.md`のIDをPR本文に記載し、概ね次の境界で分割する。

最初にworkspaceとfixture harness、次にformat parser、directory / I/O、cache / guardを独立レビューする。その後point query、range / index / BLOB、builderを追加する。圧縮とencoding、async、rkyvは各々独立PRにする。unsafe、SIMD、lock-free化は機能追加PRと分離し、導入前後の計測結果を添える。

1つの巨大PRで形式変更、cache reclamation、rkyvのunsafe accessを同時導入しない。各PRは依存先のテストと互換fixtureを保ったままmerge可能な単位にする。

## 4. 相互運用テスト

C# oracleはupstream commitを固定して別プロセスで動かす。必要な.NET SDK / package versionはharness側で固定し、Rust利用者の通常のbuildには.NETを要求しない。CIでupstreamをfloating branchから取らない。

C# builder -> Rust reader、Rust builder -> C# readerの両方向を実施する。各方向でraw values、全件走査、seed固定のランダムquery、range、prefix、count、副索引、overflowを比較する。MessagePackはDTO / codec設定を固定した意味値でも比較する。rkyvはC#側でraw bytesのみ比較する。

fixture manifestには、入力生成seed、upstream SHA、page size、encoding、filter、layout、serializer設定、期待する件数・キー・値・query結果、ファイルhashを記録する。破損fixtureには期待するエラー種別も記録する。

## 5. 性能評価

性能は「RustはC#より速い」という仮定ではなく、同条件の測定で判定する。初期のsafe・無最適化実装をRust内の基準に残し、C#版とも同じマシン・同じデータ・同じ要求結果で比較する。FFIやネットワーク往復を片側だけに追加しない。

| 軸 | 必須ケース |
| --- | --- |
| working set | CPU cache内、LLC超、DBサイズが管理cache予算の16倍以上。全RAM常駐の仮定を置かない |
| key | i64、短い / 長いascii、共通prefix衝突、UUID / ULID、hit / miss |
| query分布 | 非反復の疑似乱数、偏りあり、同一hot key、複数hot page、連続scan |
| query種別 | point、短 / 長range、prefix、count、unique / non-unique index |
| value | 小さいraw bytes、可変長値、overflow、圧縮、rkyv整列済み / copy fallback |
| 並行性 | 1 / 2 / 4 / 8 threads、同一key競合と分散key、同時missとeviction |
| cache状態 | app cache warm、app cold / OS warm、制御された環境でのOS cold、thrash |
| 起動・build | open、初回query、seed throughput、external sort、serialize、peak memory |

app cacheを空にしただけでdisk coldとは表記しない。OS cold試験でホスト全体のcacheを無断で破棄しない。隔離した測定環境で手順を記録する。

point lookupでは結果の存在だけでなく、値の同等な範囲を消費してchecksum等へ反映し、最適化による処理消去を防ぐ。乱数生成費用はDB処理と分離して報告し、両実装に同じquery列を渡す。

報告項目はthroughput、p50 / p95 / p99、allocation数・bytes、管理bufferのpeak、metadata、RSS、I/O bytes / read回数、cache hit率、page fault、build速度とfile size。mmapはmap sizeとresident setを分ける。CPU、OS、storage、toolchain、build flags、繰返し数とばらつきを保存する。

「17ns未満」等のupstream README由来の固定目標は設定しない。hot raw point / cursorの追加allocation 0は設計目標としてテストするが、cold I/O、BLOB、検証、copy fallbackまで無allocationとは呼ばない。

## 6. 安全性・メモリ試験

通常のunit / property testに加えて、parser・descriptor・flags・PageRef・compressed page・rkyv envelope / archiveをfuzzする。Miriはunsafeと借用境界を含む小さなfixtureで動かす。Loom等の並行モデル試験はcacheのpublish / retain / eviction / cancel状態を対象にする。

DBサイズとpage countを増やしても、既定backendにO(page count)のresident metadataが隠れて増えないことを確認する。長時間scan、長寿命guard、失敗I/O、圧縮bomb、巨大overflow、同時miss、validation scratchの枯渇を測る。

予算不足でエラーを返しても、既に返したValueGuardの参照が有効であり、以後の正常なqueryへ復帰できることを必須とする。memory leak、無限待機、busy loopを成功扱いにしない。

## 7. リリースゲート

対象は最初に64-bit Linux / macOS / Windowsを想定し、x86_64 / aarch64で検証する。実際のCI runnerと依存crateに基づきMSRV・対応targetをM0で確定する。32-bitやbig-endian、Wasmは検証なしに対応を宣言せず、可能な範囲でformat / codecのcross-target fixtureを追加する。

リリースには、互換性マトリクス、未対応機能、公開API例、エラーとメモリ予算の説明、unsafe一覧、ライセンス表示、再現可能なbench手順を含める。ライブラリのSemVerとDryDB format version、rkyv envelope/profile versionを別管理する。

必要な相互運用、safe APIの安全性、メモリ予算の試験が失敗している状態では、速度改善だけを理由に出荷判定しない。性能差が統計的に不明確な最適化は既定有効にしない。

## 8. 設計判断の記録

M0で決める項目は、wire仕様の未確定部分、標準filterの対応リスト、MSRV、初期のcache policy / budget、rkyv profile、queryの互換例外である。決定ごとに根拠と棄却した代案を記録する。

mmap既定化、独自format、全件indexロード、unsafeなvalidation省略は今回の初期方針には含めない。変更する場合は互換性・安全性・メモリ契約への影響を別PRで示す。
