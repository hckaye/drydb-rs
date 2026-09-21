# rkyv value codec

状態: 実装済み。`crates/drydb-rkyv`。rkyv 0.8.18で確認。[R1]

## 1. 適用範囲

rkyvは単一レコードの値をserializeし、そのarchiveを型付きで参照するために使う。DryDBのheader、catalog、page directory、B+Tree nodeはrkyvに置き換えない。DB全体を1つのarchiveにもしない。

```text
DryDB 1.4 record
  key:   upstream互換のencoding
  value: 24 bytesのenvelope + 自己完結した1つのrkyv archive
```

1つの値の中のrelative pointerは、そのarchiveの範囲だけを参照する。別レコード、別ページ、別ファイルを指すpointerは許可しない。読み出し時の検証は、ページ全体ではなくその値のbyte範囲に対して行うので、同じページ内の別の値を指すpointerも通らない。

C# readerからは、envelopeを含む値を不透明なbytesとして取得できる。rkyvのpayloadをMessagePackとして復元したり、C#のDTOとして自動でdecodeしたりはできない。型付きの相互運用が必要なtableにはrawかMessagePackを使う。

## 2. Envelope

既存formatにcodec識別fieldを追加していない。追加情報は値の中に置き、coreは解釈しない。typed tableとして明示的に読んだときだけ、adapterがenvelopeを読む。偶然magicが一致したraw valueをrkyvと自動判定することはない。

```text
0  4  magic "DRYR"
4  2  envelope version (u16)
6  2  codec profile id (u16)
8  8  schema id (u64)
16 2  schema version (u16)
18 2  flags (u16、0以外はエラー)
20 4  archive length (u32)
24 .. archive bytes
```

すべてlittle-endianで、すべて範囲検査つきで読む。archive lengthが値の残りと一致しない場合はエラーになる。この配置はgolden bytesのテストで固定してある。

schema idはアプリケーションが決める永続値で、Rustの `TypeId` や `type_name` は使わない。schema versionが一致しない値は読まない。値の暗黙のmigrationは行わないので、schemaを更新するときは旧DTOでdecodeして再seedする。

## 3. profileの検出と照合

rkyvのendianness、pointer width、alignmentはCargo featureで決まる。Cargo featureは依存グラフ全体で加算されるので、別のcrateが `big_endian` を有効にすると、こちらが書くbytesも変わる。これは防げないので、検出する。

profileは実行時に測る。pointer widthは `ArchivedUsize` のサイズ、alignmentは `ArchivedU32` のalignment、byte orderは `ArchivedU32` がrendのどの型かで決まる。いずれも型を見るだけなので、メモリを確保しない。この判定は値を最初に読むときに走り、呼び出し側が予約できる場所ではないため、1つarchiveしてbyte列を見る方法は使わない（serializerの作業領域を確保してしまう）。測った結果をenvelopeに記録し、読み出し時に現在のprofileと照合する。違えば `SchemaMismatch` になる。

既定のprofile（rkyv 0.8、little-endian、4 bytes pointer、aligned）から外れていれば、unit testが落ちる。

## 4. alignmentとcopy

DryDBの値の開始位置は、rkyvが要求するalignmentを保証しない。page bufferの先頭を整列させても、可変長キーとenvelopeの後ろにあるarchiveが整列するとは限らない。値がページ内のどこに来るかはbuilderのpackingが決めるので、どちらの実装からも制御できない。

読み取りの手順は次のとおり。

1. coreから `ValueGuard` を得て、envelope、profile、schema、archiveの正確な範囲を確認する。
2. archiveの先頭アドレスが必要なalignmentを満たしていれば、そのsliceを検証して借用する。
3. 満たしていなければ、その1値だけを整列したbufferへcopyして検証する。bufferは予算へ計上する。
4. copy禁止modeでは `Unsupported` を返す。無条件のpointer castで回避することはしない。

要求するalignmentは、rootの `align_of` ではなく16 bytesを既定にする。rootの `align_of` が4でも、archiveの中に64-bit値や128-bit値が入りうる。rkyvは各objectをbuffer先頭からの相対位置で自然に整列させるので、buffer先頭が16 bytes境界にあれば内部も揃う。この判断は最初rootだけを見る実装にしていて、`Vec<u64>` を含む型で実際に検証が落ちたので変えた。16 bytesより強いalignmentを要求する型は `Codec::require_alignment` で指定でき、上限は64 bytesになる。

### 「ゼロコピー」の範囲

| 経路 | 実際の動作 |
| --- | --- |
| 無圧縮、cache hit、archiveが整列済み | page bytesを借用する。値のcopyも通常のRust値への復元もしない |
| cache miss | fileからpage bufferへのreadは必要になる。型付きアクセスの追加copyとは別の話 |
| 圧縮ページ | 復号bufferが必要になる。復号後のarchiveが整列していれば追加copyは無い |
| alignmentが合わない | その1値分のcopyを伴う |
| owned DTOへの変換 | 明示的なdeserializeと、所有する型が必要とする確保を伴う |

全件をdeserializeしないこと、1値のdeserializeを避けること、I/Oも含めて一切copyしないことは、それぞれ別の話になる。

## 5. 借用と検証のAPI

```rust
let raw = table.get(key)?.expect("key present");
let prepared = codec.prepare::<Monster>(raw)?;
let archived = prepared.access()?;   // &ArchivedMonster、preparedからの借用
```

`prepare` は元の `PagePin` か、整列したowned bufferを保持する。`access` から得た参照は `prepared` より長生きできないので、cacheの退避や `Database` のdropでdanglingにはならない。

`access` は呼ぶたびにrkyvのcheckedなAPIで検証する。一度得た参照で複数のfieldを読める。検証結果の再利用はしていない。再利用する場合、検証済みという事実を具体的なallocation、範囲、schema、型、profileに結び付ける必要があり、ページ番号だけのglobalなフラグでは足りない。

`access_unchecked` は使わない。検証対象はその1値のarchiveのsliceだけで、ページ全体は渡さない。

## 6. 制限

`Codec::max_value_bytes` がserializeとprepareの両方に効く。alignmentのためのcopyは、`Codec::budget` にdatabaseと同じ予算を渡せばそこに計上され、入らなければ `BudgetExceeded` になる。

rkyvのvalidatorが持つ以上のDoS耐性は主張しない。検証は深さと作業量の上限をvalidatorの既定に委ねている。

## 7. serialize

シード時は各値を独立にserializeし、envelopeを付けてcore builderへ渡す。`rkyv::to_bytes` は呼び出しごとに新しいserializerを作るので、shared pointerの状態やarena、serializerの位置がレコード間で持ち越されることはない。

## 8. テスト

`crates/drydb-rkyv/tests/codec.rs` が次を扱う。envelopeの往復、schema idとversionの不一致、profileの不一致、raw値をarchiveと誤認しないこと、あらゆる長さでの切り詰め、archive byteのランダムな書き換え、隣接する値を指すpointer、databaseへ格納した値の往復とraw bytesの一致、借用とcopyの判定がアドレスのalignmentと一致すること、copy禁止modeの動作、copyの予算計上。

`fuzz/fuzz_targets/rkyv_value.rs` は、任意のbyte列をそのまま、および整合したenvelopeで包んだ形の両方で流し込む。

## 参考

- [R1: rkyvの機能・format control・互換性](https://docs.rs/rkyv/0.8.18/rkyv/)
- [R2: rkyv validation](https://rkyv.org/validation.html)
- [R3: rkyv 0.8.18 access API](https://docs.rs/rkyv/0.8.18/rkyv/fn.access.html)
- [coreのメモリ・所有権契約](architecture.md)
