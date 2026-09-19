# rkyv value codec設計

状態: Proposed / 実装前。調査日: 2026-09-20。調査したAPI系統はrkyv 0.8、docs.rsの表示版は0.8.18。[R1]

## 1. 適用範囲

rkyvは**単一レコードのvalue**をserializeし、そのarchiveを型付きで参照するために使う。DryDBのheader、catalog、page directory、B+Tree nodeをrkyvへ置き換えない。全DBを`ArchivedHashMap`へ変換しない。

```text
DryDB 1.4 record
  key: upstream互換encoding
  value: codec envelope + one self-contained rkyv archive
```

1 value内のrelative pointerはそのarchiveの範囲内だけを参照する。別レコード、別page allocation、別ファイルへのrkyv pointerを許可しない。複数レコード間の共有は通常のキー・IDで表現する。単一巨大archiveからDB全体へ参照を広げる設計は採用しない。

C# readerからはenvelopeを含むvalueを不透明なbytesとして取得できる。**rkyv payloadをMessagePackとして復元したり、C#のDTOとして自動decodeしたりはできない。** 型付き相互運用が必要なtableにはraw / MessagePackを使う。

## 2. Envelopeとschema

既存formatにcodec識別fieldを追加しない。追加情報はvalue内に置き、coreは解釈しない。typed tableを明示的に開いたときだけadapterがenvelopeを読む。偶然一致するmagicから任意のraw valueをrkyvと自動判定しない。

envelopeに必要な項目は以下。具体的なmagic、幅、offset、version値は `R02` でwire仕様とfixtureを確定してから実装する。

| 項目 | 意味 |
| --- | --- |
| envelope version | このwrapperの形式。DryDB format versionと独立 |
| codec / archive profile ID | rkyvの互換系列、endianness、pointer width、alignment設定を識別 |
| schema ID / schema version | アプリケーションが定義する永続ID。Rustの`TypeId`や`type_name`を永続化しない |
| archive offset / length | envelope内の有効archive範囲。alignment paddingを含めて境界を確認 |
| root locationの規則 | 初期案は通常のrkyv末尾root。別位置を許すならprofileを分ける |

schema fingerprintを補助情報として使っても、型・メモリ安全性の証明にはしない。値の暗黙migrationは行わず、schema更新は旧DTOでdecodeして再seedする。アプリ側にはどのschemaとcodecを使うか明示させる。

## 3. 初期profile

初期の候補は **rkyv 0.8系 / little-endian / pointer-width 32 / aligned / bytecheck有効**。実装時の基準patchを固定し、更新時にarchive互換テストを実行する。

rkyvのformat controlはシリアライズ形式を変え得る。互換性はschema、format設定、rkyvのsemver互換性に依存する。[R1] `drydb-rkyv`は対応profileを明示するライブラリとし、利用側のCargo feature統合が別のprofileを暗黙に作らないよう、feature構成とcompile-time / 起動時検査を設計する。

aligned / unaligned、異なるpointer-widthを同じrkyv型定義で同時に読めるとは約束しない。代替profileの比較は別buildで行う。互いに排他的なfeatureを単に`--all-features`で混ぜるCIにはしない。

pointer-width 32は**各archiveの制限**であり、DryDBファイル全体を32-bit offsetにすることではない。大きすぎる1値のserializeはエラーにする。ファイル側のoffsetやpage countには別途upstreamの制約を適用する。

## 4. アラインメントとコピー条件

DryDBの既存value開始位置は、rkyvの要求alignmentを保証しない。page bufferの先頭を整列しても、可変長キー・metadata・envelopeの後のarchiveが整列するとは限らない。圧縮後のfile offsetと解凍後のpayload offsetも別である。

初期実装の読取手順は次とする。

1. coreから`ValueGuard`を得て、envelope、profile、schema、archiveの正確なslice範囲を確認する。
2. 実アドレスと対象型の条件が合えば、そのsliceを検証して借用する。
3. 条件が合わない場合は、**その1値だけ**を適切なalignmentを持つowned bufferへcopyして検証する。bufferは管理メモリ予算へ計上する。
4. copy禁止モードでは`AlignmentRequired`等の明示的エラーを返す。無条件のpointer castでは回避しない。

単にrootの`align_of`を見るだけで、全relative pointer先の整列や範囲の検証を代用しない。1値単位で独立にserializeし、relocation先でも必要な内部alignmentが保たれる条件を確認する。

追加最適化として、既存のvalue bytes内のpaddingによる配置調整と、unaligned profileを評価する。ただし、page packingと副索引PageRefを壊さず、custom archived typeのalignmentも検証する。`unaligned`というfeature名だけで任意のschemaのalignment問題がなくなるとは扱わない。

### ゼロコピーという表現の範囲

| 経路 | 実際の動作 |
| --- | --- |
| 無圧縮・cache hit・archive整列済み | page bytesを借用。valueの追加copy・通常のRust値への復元を避けられる |
| cache miss | fileからpage bufferへのreadは必要。typed accessの追加copyとは別 |
| 圧縮ページ | 解凍bufferが必要。解凍後のarchiveが整列していれば追加copyを避けられる |
| alignment不一致 | 1値分のcopyを伴う。厳密なzero-copyとは呼ばない |
| owned DTOへの変換 | 明示的deserializeと必要なallocationを伴う |

全件デシリアライズしないこと、1値のデシリアライズを避けること、I/Oも含めて一切copyしないことを混同しない。

## 5. 借用・検証API

初期APIは、安全な`rkyv::access`を使ってarchiveを検証し、所有bufferに束縛された参照を返す。[R2][R3]

```text
raw = table.get(key)?
prepared = RkyvValue<T>::prepare(raw, schema, limits)?
archived = prepared.access()?  // &Archived<T>; preparedからの借用
use archived fields
```

これはAPIの概略であり、利用可能なRustコードではない。`prepare`は元のPagePinまたは整列済みowned bufferを保持する。`access`から得た参照はprepared valueより長生きできず、cache evictionやDatabaseのdropでdanglingにしない。self-referential structを無造作に作らない。

初期経路では参照取得ごとにsafe validationを行い、一度得た参照で複数fieldを読めるようにする。検証結果の再利用は実測後の最適化とする。

再利用する場合も、検証済みという事実は具体的な不変allocation、slice範囲、schema、型、profileに結び付ける。page ordinalだけのglobalフラグや、別型へ流用できる証明にはしない。cache eviction / 再ロード / 世代変更で失効し、検証cacheもboundedにする。unsafeな再参照を導入する場合は専用レビューとMiriテストを要求する。

## 6. 未信頼入力

safe APIではenvelopeの整合性に加えてarchive構造を検証する。`access_unchecked`を既定経路にしない。ハッシュや署名の一致は、schema一致やrelative pointerの妥当性を自動的には保証しない。

検証対象は対象1値のarchive sliceのみであり、ページ全体やDB全体を検証領域として渡さない。別の値を指すpointerが「ページ内だから有効」と通過することを防ぐ。pointer範囲、alignment、UTF-8、enum discriminant、shared pointerの型などを検証する。[R2]

`max_value_bytes`、validation scratch、作業量、深さの制限も設ける。必要な制限を標準validatorだけで表現できない場合は、supported schemaやcustom contextを限定し、未実装のDoS耐性を主張しない。coreの構造検証とcodec検証は別レイヤーとする。

## 7. serialize経路

シード時は各値を独立にserializeし、envelopeを付けてcore builderへ渡す。shared pointer状態やserializer位置をレコード間で誤って持ち越さない。scratch再利用は可能でも、archiveの独立性を維持する。

並列serializeではworker数と総scratchを予算化し、入力順が必要なbuilderへbounded queueで渡す。1巨大値で全予算を使う場合は明示的にエラーにする。全レコードのarchiveを`Vec`へ貯めてから書くことは必須にしない。

通常の`String` / `Vec` / enum / nested structと、alignmentが強いcustom typeを検証対象にする。rkyvの最適化はserialize throughput、validate cost、archive access、owned deserializeを分けて測定する。「rkyvだから常に最速」とは扱わない。

## 8. 必須テスト

`R01`〜`R06`に次を割り当てる。任意offset配置、圧縮後の再配置、inline / overflow、schemaとprofile不一致、切り詰め、無効relative pointer、別レコードへのpointer、UTF-8とenum破損、page eviction、guard drop、予算不足、32-bit / 64-bit target間のfixtureを含める。

C#側ではrkyv値をraw bytesとして往復し、envelopeを含めてbytesが保存されることを確認する。これはC#の型付きrkyv decoderのテストとは呼ばない。

## 参考

- [R1: rkyvの機能・format control・互換性](https://docs.rs/rkyv/0.8.18/rkyv/)
- [R2: rkyv validation](https://rkyv.org/validation.html)
- [R3: rkyv 0.8.18 access API](https://docs.rs/rkyv/0.8.18/rkyv/fn.access.html)
- [coreのメモリ・所有権契約](architecture.md)
