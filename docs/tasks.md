# タスクリスト

状態: 全タスク未着手。設計文書の追加と、以下の実装完了を混同しない。

P0 = 互換性・安全性・基本機能のブロッカー。P1 = 対応範囲の完成。P2 = 計測後の追加最適化。

各タスクはPR本文からIDで参照する。依存は前提となるタスクID。チェックを付ける条件は記載された成果物とテストの確認であり、コードが存在するだけでは完了にしない。

## M0: 基準と基盤

- [ ] **C01 / P0 — upstreamを固定してwire仕様を確定する。** 依存: なし。Header、descriptor、PageRef、node metadata、flags、sentinel、filter framing、duplicate key encodingをoffset・幅・意味付きで記録する。根拠commitと未確定項目を残す。
- [ ] **C02 / P0 — C# fixture generator / reader oracleを作る。** 依存: C01。固定SDK・upstream SHAから再生成でき、入力・設定・出力hashのmanifestが一致する。Rustの通常buildには.NETを不要とする。
- [ ] **C03 / P0 — query境界と互換例外を確定する。** 依存: C02。空・無限端・排他端・prefix・i64極値・重複副索引・non-unique Getの戻り値を実測し、期待結果と疑義を台帳化する。
- [ ] **C04 / P0 — fixture matrixを揃える。** 依存: C02, C03。全node layout、複数table、副索引、overflow、長さ境界と破損入力に期待結果またはエラーが付いている。
- [ ] **F01 / P0 — Rust workspaceとCI方針を定める。** 依存: なし。MSRV、edition、target、fmt / clippy / test、optional featureの組合せを固定する。upstream由来コードのライセンス保持方針を実装する。
- [ ] **F02 / P0 — 共通の予算・診断・ベンチ基盤を作る。** 依存: F01。allocation capacity、I/O、cache、peak memoryを観測できる。固定workload manifestをC# / Rustで共有する。
- [ ] **R01 / P0 — rkyvと所有権の技術spikeを行う。** 依存: F01。0.8系profile、任意offset、強いalignment、value単位のrelocation、safe accessの借用を小さなテストで確認し、採用profileとfallbackを記録する。

## M1: 無圧縮reader

- [ ] **D01 / P0 — safe format parserを実装する。** 依存: C01, C04, F01。checked decodeで全layoutを処理し、version・flags・サイズ・参照の破損をpanic / UBなしに拒否する。
- [ ] **D02 / P0 — positional PageSourceを実装する。** 依存: D01, F02。short read、EOF、I/O失敗に対応し、共有Seek lockを使わず並行readの正しさを検証する。
- [ ] **D03 / P0 — bounded PageDirectoryを実装する。** 依存: D01, D02。必要chunkのみ読み、page count分の常駐配列を作らずにordinal -> offsetを解決する。
- [ ] **D04 / P0 — PageBuffer / PagePin / ValueGuardを実装する。** 依存: F02, D02。guard保持中のeviction・DB dropでもsliceが有効。compile-fail / Miriで寿命境界を検証する。
- [ ] **D05 / P0 — bounded cacheとsingle-flightを実装する。** 依存: D03, D04。安全な所有権取得、予約、失敗cleanup、並行miss、退避後pinの予算計上がテストされる。
- [ ] **D06 / P0 — B+Treeのpoint lookupを実装する。** 依存: C04, D01, D05。i64 / ascii、全layout、digest衝突、hit / missがC# fixtureと一致する。
- [ ] **D07 / P0 — メモリ上限の不変条件を検証する。** 依存: D05, D06。DBがcache予算の16倍以上でも全件ロードせず検索でき、guard保持時の不足は無限待機でなく明示的エラーになる。

## M2: query・副索引・BLOB

- [ ] **Q01 / P0 — borrowed cursorを実装する。** 依存: D06。seek / advance / current、昇降順、葉間移動、省略キー復元を扱い、scan結果を自動的に全件Vec化しない。
- [ ] **Q02 / P0 — range / prefix / countを実装する。** 依存: C03, Q01。包含・排他・無限端・空prefixがoracleと一致し、countでvalueやcodecを読まない。
- [ ] **Q03 / P0 — unique / non-unique副索引を実装する。** 依存: C03, Q01, Q02。PageRefとduplicate key規則を照合し、参照先値・同値順序・排他境界が期待どおりになる。
- [ ] **Q04 / P0 — overflowと無圧縮BLOB streamingを実装する。** 依存: D05, D06。inline切替、巨大値、壊れた参照をテストし、streamingでは全BLOBを確保しない。
- [ ] **Q05 / P0 — reader意味論の差分テストを自動化する。** 依存: Q01, Q02, Q03, Q04。乱数seed付きquery列をC# oracleと比較し、差分をfixtureとして保存する。

## M3: builderと双方向互換

- [ ] **B01 / P0 — sorted streaming builderを実装する。** 依存: C01, D01, Q05。葉・内部node、root patch、directoryを生成し、空入力・重複・サイズ上限・失敗を扱う。
- [ ] **B02 / P0 — spoolとexternal sortを実装する。** 依存: B01, F02。未ソートseed、副索引PageRef、directory offsetを総メモリ予算付きで処理し、全件常駐を不要にする。
- [ ] **B03 / P0 — 副索引・overflow writerを完成させる。** 依存: B01, B02, Q03, Q04。全対応layout・複数table・重複副索引・巨大値をC# readerで参照できる。
- [ ] **B04 / P0 — 無圧縮の双方向interopをCI化する。** 依存: C02, B03。C#->Rust / Rust->C#でbytes、結果列、countが一致する。Rust側の固定設定buildの再現性も確認する。
- [ ] **B05 / P0 — 一時ファイルと公開処理をhardeningする。** 依存: B02, B03。途中失敗・cancel・容量不足で既存DBを破壊せず、cleanupとOS別flush / publish保証を文書化する。

## M4: 互換範囲の拡張

- [ ] **X01 / P1 — 標準encodingと拡張registryを揃える。** 依存: C01, B04。UUIDv7 / ULID等のID・byte order・digest vectorsを照合し、unknown IDを明示的に拒否する。
- [ ] **X02 / P1 — 標準page filterを移植する。** 依存: C01, B04。対応filterを列挙し、framing・処理順・BLOB適用を双方向fixtureで検証する。解凍サイズ・scratchに上限を設ける。
- [ ] **X03 / P1 — MessagePack adapterを追加する。** 依存: B04。DTOごとに配列 / map・整数・extension等を固定し、C#型との相互運用を検証する。任意DTO互換を宣言しない。
- [ ] **X04 / P1 — async adapterを実装する。** 依存: Q05, D05。runtime依存をoptional化し、blocking経路・cancel・single-flight待機者・予算予約を試験する。
- [ ] **X05 / P1 — CLI inspect / verify / build / queryを追加する。** 依存: B05, Q05。全件verifyはstreamingで実行し、破損位置・対応format・codecを診断できる。buildは予算付き入力を使う。
- [ ] **X06 / P1 — 拡張後の互換性マトリクスを公開する。** 依存: X01, X02, X03, X04, X05。各対応項目がCI fixtureへ対応し、custom拡張・Unity・旧formatの対象外を明記する。

## M5: rkyv統合

- [ ] **R02 / P1 — envelope / schema / profile仕様を確定する。** 依存: R01, C01。field幅・byte order・root規則・offset・length・versionを文書とgolden bytesに固定する。Cargo feature衝突を検出する。
- [ ] **R03 / P1 — レコード単位serializerを実装する。** 依存: R02, B03。scratch再利用・並列化を予算内に収め、レコード間relative pointerやshared状態の漏れを防ぐ。
- [ ] **R04 / P1 — alignment対応の借用readerを実装する。** 依存: R02, D04, Q04。整列時は追加copyせず、不整列時は1値だけcopyする。copy禁止時と予算不足時は明示的に失敗する。
- [ ] **R05 / P0 — 型・検証・寿命をhardeningする。** 依存: R03, R04。schema / profile不一致、別値へのpointer、壊れたarchive、guard寿命、eviction、強いalignmentをsafe APIから安全に扱う。検証cacheは初期必須にしない。
- [ ] **R06 / P1 — rkyvの統合fixtureと計測を揃える。** 依存: R05, X02, B04。副索引・圧縮・overflow経由の型付き参照、C#のraw byte往復、cross-target fixtureを確認。serialize / validate / access / deserialize / copy費用を分離報告する。

## M6: 計測・最適化・リリース

- [ ] **P01 / P0 — 再現可能なbaselineを保存する。** 依存: F02, B04。C# / Rustの同一workloadを測定し、cold / warm、同一key / 分散key、allocation、RSS、I/O、p99を保存する。
- [ ] **P02 / P2 — 安全なhot path最適化を評価する。** 依存: P01, Q05。encoding特殊化、borrow再利用、bounded root保持、cache policyを個別比較し、性能根拠とmemory差を記録する。
- [ ] **P03 / P2 — SIMD / branchless経路を評価する。** 依存: P02, H01。scalar版とのproperty test、CPU検出・fallback、端点・衝突・padding試験を通し、実測で有効な経路だけ採用する。
- [ ] **P04 / P2 — atomic / lock削減を評価する。** 依存: P02, H02。reclamation設計、同一hot key競合、eviction中readを検証する。raw pointer load後の無保護retainを禁止する。
- [ ] **P05 / P2 — mmap / dense directory / prefetchを比較する。** 依存: P01, H01, D07。opt-inで別々に比較し、file不変性の安全性契約と管理予算 / OS residencyの差を文書化する。
- [ ] **P06 / P2 — rkyv追加最適化を評価する。** 依存: R06, P01, H01。unaligned profile、value padding、検証再利用を別build / 別PRで比較し、安全性・形式互換・コピー削減を個別に証明する。
- [ ] **H01 / P0 — property / fuzz / Miriを継続実行する。** 依存: D01, D04。まずparserとguardから開始し、Q / X / R実装後に対象を追加する。リリース時にはcodec・圧縮・unsafe箇所を含む全対象が合格している。
- [ ] **H02 / P0 — 並行性・資源枯渇試験を継続実行する。** 依存: D05。single-flight、publish / eviction、長寿命guard、I/O失敗、cancelをmodel testとstress testで確認する。後続async / rkyv経路も追加する。
- [ ] **H03 / P1 — リリース文書とfeature / target CIを完成させる。** 依存: X06, R06, H01, H02, P01。対応範囲、MSRV、メモリ契約、unsafe一覧、ライセンス、実行可能な利用例とbench手順を公開する。P2最適化はリリース必須条件にしない。

## 完了判定の共通ルール

全タスクで関連する既存fixtureを保つ。速度改善だけで互換性・安全性・メモリ予算の失敗を上書きしない。upstreamの疑義やplatform制約はテストの無効化で隠さず、[互換性仕様](compatibility.md)と例外台帳へ記録する。
