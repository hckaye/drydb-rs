# 互換性仕様・upstream調査記録

状態: Proposed / 実装前。調査日: 2026-09-20。

## 1. 互換基準

対象は `hadashiA/DryDB` の commit **`6b175929491793948e63430c20c2d6f58300d97f`**（2026-09-09）、storage format **1.4** とする。[U1][U2]

upstreamのbranch名やNuGetの最新版を、CIの暗黙の互換基準にしない。fixtureにはcommit、生成SDK、設定、入力、期待値、SHA-256を記録する。upstream更新は、差分調査とfixture再生成を伴う独立PRにする。

本書はソースから確認した事項と実装方針を記録するもので、全フィールドを確定した完全なバイナリ仕様ではない。残りの仕様抽出と実行による確認は `C01`〜`C04` の成果物とする。現段階で相互運用を実行検証したとは主張しない。

## 2. 互換性のレベル

| レベル | 必須となる検証 |
| --- | --- |
| ファイル読取互換 | C# builderの出力をRustが読み、各キー・値・索引を正しく参照できる |
| ファイル書込互換 | Rust builderの出力を固定したC# readerが読み、同じデータを参照できる |
| 検索意味論互換 | 点検索、範囲、prefix、昇降順、count、副索引の結果・境界が一致する |
| 値の互換 | raw bytesは同一。MessagePack等の型付き互換はDTOとcodec設定ごとに確認する |
| API対応 | Rustの所有権に合わせたAPIを提供する。C#のメソッド名・型・非同期モデルの逐語移植は要求しない |

「ファイル互換」は、同じ論理データから生成したファイル全体がC#とbyte-for-byteで等しいことまでは意味しない。有効なページ分割・配置の違いは許す。固定入力・固定設定でのRust builder自身の再現性は別途テストする。

## 3. ソースで確認した形式

### ファイルヘッダとページ参照

`DryDBCodec.Decode.cs` のHeaderは26 bytes、little-endian。フィールドは次のとおり。[U2]

| Offset | 幅 | 内容 |
| --- | --- | --- |
| 0 | 4 | magic `DRY\0` |
| 4 | 1 | major version = 1 |
| 5 | 1 | minor version = 4 |
| 6 | 2 | page filter count |
| 8 | 4 | page size（signed int） |
| 12 | 2 | table count |
| 14 | 4 | page count（signed int） |
| 18 | 8 | page directory position（signed long） |

ヘッダ後にfilter ID、table/index descriptor、ページ群、末尾のpage directoryが置かれる。ページの参照値はファイルオフセットではなく**dense page ordinal**。directoryの各8-byte offsetを介して実際の位置を得る。root、兄弟、子、overflow、副索引の参照を同じ規則で扱う。[U2][U3]

C# readerは1.4以外を拒否する。Rust側も初期実装で旧1.0〜1.3や未知の将来版を推測して読まない。unknown encoding/filter IDも、別の実装へ黙って置換せずエラーにする。

### B+Treeのレイアウト

全tree pageにorder-preservingな64-bit digest列がある。digestは一般のhashではなく、キー順序を保つ検索補助情報である。衝突時は完全キーで比較する。[U4][U6]

| 形式・フラグ | 確認事項 |
| --- | --- |
| Node kind | 下位8 bitsがLeaf / Internal |
| bit 8 | 旧HasKeyDigests。1.4では廃止され、digestが必須 |
| `EytzingerDigests` / bit 9 | digestをBFS順の完全二分木として格納。padding・実エントリの識別が必要 |
| `CompactMeta` / bit 10 | compact offset列を使用。builderではpage size <= 32767のとき採用 |
| `OmittedKeys` / bit 11 | exact digestからキーを復元。CompactMetaと併用し、Eytzingerとは併用しない |

`OmittedKeys`の内部nodeには通常のキーpayloadやmetadataがない。ファイル内のフラグを無視して単一レイアウトとして読む実装は不可。既存レイアウトをそのまま読み、Rust最適化のために非互換のfieldを追加しない。[U4][U5]

### キー、副索引、overflow

`i64`はLEのsigned整数比較で、digestは符号bitを反転した値。`ascii`はバイト辞書順で、先頭最大8 bytesからdigestを生成し、衝突時は完全比較する。`uuidv7`とcustom registryも存在する。ULID等の追加パッケージを含め、ID・バイト順・文字列変換はfixtureで確定する。[U6]

副索引は主レコードの値を複製するのではなく、ページ番号・開始位置・長さを含む`PageRef`を持つ。RustではC#のnative struct castを再現せず、確定したwire layoutを明示的にdecodeする。[U3][U8]

overflow化は値が大きい場合だけでなく、ページ内に収まるかどうかやsentinelとの衝突にも依存する。READMEの大きなBLOBの説明だけで閾値を決めない。`TreeBuilder`と`LeafNodeReader`に基づくinline / overflow両経路のfixtureが必要。[U5]

### メモリ・キャッシュに関する差異

C#版はS3-FIFOキャッシュを持ち、page countに比例するentry配列、ghost epoch配列、offset配列を使用する。また、GC管理のentryに対する参照カウント操作を前提としている。[U7]

**これらはファイル互換の条件ではない。** Rust版の既定経路ではbounded directory cacheとresident pageに比例するcache metadataを使う。GC前提のoptimistic retainは直訳しない。詳しくは[アーキテクチャ](architecture.md)を参照。

## 4. 対応マトリクスと段階的な名称

以下はすべて計画であり、現在は未実装。

| 項目 | 到達段階 | 互換の確認方法 |
| --- | --- | --- |
| 1.4、無圧縮、複数table、raw value、i64 / ascii | M1読取・M3書込 | 双方向fixture、全件照合、ランダム検索 |
| classic / compact / omitted / Eytzinger各有効構成 | M1読取・M3書込 | C#生成fixtureを全構成で照合 |
| range / prefix / count / cursor / asc / desc | M2 | 境界と結果列をC# oracleと比較 |
| unique / non-unique副索引、overflow | M2読取・M3書込 | PageRef解決、重複、順序、境界、BLOB照合 |
| UUIDv7 / ULID、encoding拡張 | M4 | 生成元のencoding ID・比較・digest・復元のvectors |
| upstream標準page filters / 圧縮 | M4 | IDと処理順、framing、双方向圧縮fixture |
| sync / asyncの検索結果 | M1〜M4 | 同じ意味論。asyncのruntime結合は任意機能 |
| MessagePack型付き値 | M4 | DTO、配列/名前付きmap、整数、extension表現を明示 |
| rkyv値 | M5 | core上はraw bytes互換。型付き参照はRust側の追加機能 |
| C#利用者の任意custom filter / encoding | 拡張点のみ提供 | 同じID・意味論のRust実装を登録した場合のみ対応 |
| Unity固有loader / IL2CPP統合 | 初期対象外 | Rustコアのファイル互換と区別する |

M1〜M3を「無圧縮コア互換」と呼び、圧縮や全encodingまで対応したように表現しない。M4後もサポートする標準プラグインの具体的リストを公開する。任意のC#拡張を自動実行できるという意味での「完全互換」は主張しない。

## 5. 意味論の確定が必要な境界

`C03`で以下を実行fixture化する。

- 空DB / 空table / 空キー / 空値、キー不存在、無限端、逆転範囲、両端の包含・排他、prefixの空・末尾0xff・共通8-byte prefix。
- i64のMIN / MAX / 負値、ASCII文字列入力の置換規則とraw byte入力の違い、UUID / ULIDのバイト順。
- 非ユニーク副索引のduplicate suffix、同値内の順序、排他境界、point lookupの戻り値。`NonUniqueSecondaryIndexQuery.Get`と`GetRange`は参照解決経路が異なるように見えるため、ソースの見た目だけで結果を決めない。[U9]
- page size 32767 / 32768、digest最大値とEytzinger padding、inlineとoverflowの切替、値長65534 / 65535 / 65536、キー長・ページ数等の上限。
- filterの順序、length prefixが表すサイズ、BLOBへの適用、空rootや兄弟参照のsentinel、descriptorと`PageRef`の厳密なbyte layout。

Rustの公開APIは`Bound`等で無限端を明示し、空byte列を無条件に無限端へ変換しない。C#互換adapterが必要なら、曖昧な挙動を名前付きで隔離する。

## 6. 不具合・仕様差の扱い

有効なファイルと意図された検索意味論の互換を目標とする。未定義動作、解放済みメモリ参照、入力長を無視した読取などの安全性問題を移植してはならない。

oracleとの差が見つかった場合は、入力・upstream SHA・C#実測結果・Rustの期待結果・判断理由を例外台帳に記録する。都合の悪いfixtureを黙って削除しない。仕様が未確定の機能は未対応と表示し、意味論の差を解決または明示するまで当該機能の互換完了判定を保留する。

## 7. 一次資料

以下のupstreamリンクはすべて同一commitに固定する。

- [U1: 対象commit](https://github.com/hadashiA/DryDB/commit/6b175929491793948e63430c20c2d6f58300d97f)
- [U2: DryDBCodec.Decode.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/DryDBCodec.Decode.cs)
- [U3: DryDBCodec.Encode.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/DryDBCodec.Encode.cs)
- [U4: BTree/NodeHeader.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/BTree/NodeHeader.cs)
- [U5: BTree/TreeBuilder.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/BTree/TreeBuilder.cs)
- [U6: IKeyEncoding.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/IKeyEncoding.cs)
- [U7: Internal/PageCache.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/Internal/PageCache.cs)
- [U8: Internal/PageRef.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/Internal/PageRef.cs)
- [U9: NonUniqueSecondaryIndexQuery.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/NonUniqueSecondaryIndexQuery.cs)

本書内の`[U番号]`はこの資料一覧を指す。公開READMEの速度値はRust版の性能根拠に使わない。
