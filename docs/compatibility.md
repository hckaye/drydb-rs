# 互換性仕様・upstream調査記録

状態: 実装済み。相互運用テストで確認済み。最終更新: 2026-09-20。

## 1. 互換基準

対象は `hadashiA/DryDB` の commit **`6b175929491793948e63430c20c2d6f58300d97f`**（2026-09-09）、storage format **1.4**。[U1][U2]

CIは `tests/interop/fetch-upstream.sh` でこのcommitを取得する。upstreamのbranch名やNuGetの最新版は使わない。upstreamを更新するときは、差分調査とfixture再生成を伴う独立PRにする。

本書に「確認済み」と書いた項目は、upstreamのソースを読んだうえで、C#実装とRust実装の双方でファイルを生成し、双方で読み出して結果を比較する自動テストが通っていることを指す。テストは `crates/drydb-interop` にあり、`tests/interop/fetch-upstream.sh` を実行したあと `DRYDB_INTEROP=1 cargo test -p drydb-interop` で走る。.NET SDKがない環境では、このテストだけがスキップされる。

## 2. 互換性のレベルと到達状況

| レベル | 状況 | 確認方法 |
| --- | --- | --- |
| ファイル読取互換 | 確認済み | C# builderが生成した12件のfixtureをRustが読み、全件走査・点検索・範囲・count・副索引の結果がC#と一致する |
| ファイル書込互換 | 確認済み | Rust builderが生成した同じ12件をC# readerが読み、同じ比較が通る |
| 検索意味論互換 | 確認済み（例外は§6） | 点検索、範囲、昇降順、count、副索引について、境界の組合せを網羅した問い合わせ列を双方で実行して比較する |
| 値の互換 | raw bytesは一致。MessagePackはfixture DTOで確認済み | §5 |
| API対応 | Rustの所有権に合わせた別APIを提供する | C#のメソッド名・型・非同期モデルの逐語移植はしない |

「ファイル互換」は、同じ論理データから生成したファイル全体がbyte-for-byteで等しいことまでは要求しない。ただし実際には、12件のfixtureのうち10件は両実装の出力が完全に一致した。一致しない2件の原因は§6のD5とD9で、どちらも意図した設計上の差である。

## 3. ソースで確認した形式

### ファイルヘッダ

`DryDBCodec.Decode.cs` のHeaderは26 bytes、little-endian。[U2]

| Offset | 幅 | 内容 |
| --- | --- | --- |
| 0 | 4 | magic `DRY\0` |
| 4 | 1 | major version = 1 |
| 5 | 1 | minor version = 4 |
| 6 | 2 | page filter count（u16） |
| 8 | 4 | page size（i32） |
| 12 | 2 | table count（u16） |
| 14 | 4 | page count（i32、後方patch） |
| 18 | 8 | page directory position（i64、後方patch） |

ヘッダの直後にfilter ID（1 byte長 + UTF-8）、table descriptor群、ページ群、末尾のpage directoryが並ぶ。

### table descriptorとindex descriptor

```text
Table         name_length(i32) name(utf8)
              IndexDescriptor                (primary key)
              index_count(u16)
              IndexDescriptor[index_count]   (secondary keys)

IndexDescriptor
              name_length(u16) encoding_id_length(u16)
              name(utf8) encoding_id(utf8)
              is_unique(u8) value_kind(u8) root_ordinal(i64、後方patch)
```

`value_kind` は0がRawData、1がPrimaryKey、2がPageRef。primary keyはRawData、副索引はPageRefを使う。primary keyのindex名はC# builderが `{table}_pk` を生成するので、Rust builderも同じ名前を書く。

### ページ参照はordinal

ページの参照値はファイルオフセットではなく、flush順に割り当てられるdense page ordinal。末尾のpage directoryが各ordinalの8-byte file offsetを持つ。root、左右の兄弟、子、overflow、副索引のPageRefすべてが同じ規則に従う。`-1` は「参照先なし」を表す。[U2][U3]

C# readerは1.4以外を拒否する。Rust readerも同じで、未知のencoding IDやfilter IDは別の実装へ読み替えずエラーにする。

### ページとB+Treeのレイアウト

```text
0  4   page_length (i32、この4 bytesを含むページ全体の長さ)
4  4   kind | flags (i32)
8  4   entry_count (i32)
12 8   左兄弟のordinal (i64、-1でなし)
20 8   右兄弟のordinal (i64、-1でなし)
28 ..  digest配列
..     entry metadata
..     entry payload
```

先頭28 bytesはpage filterを適用しても生のまま残る。これがあるので、圧縮済みのページに対しても右兄弟ポインタを後から書き込めるし、readerは4 bytes読むだけでページの格納長が分かる。

flagsの割当は次のとおり。

| ビット | 意味 | 確認事項 |
| --- | --- | --- |
| 下位8 bits | Leaf(0) / Internal(1) | それ以外はエラー |
| bit 8 | 旧HasKeyDigests | 1.4では書かれない。立っていればエラー |
| bit 9 | EytzingerDigests | digestをMaxValueで埋めた完全二分木のBFS順で格納 |
| bit 10 | CompactMeta | metadataをu16のoffset配列にする。builderはpage size 32767以下で採用 |
| bit 11 | OmittedKeys | キーのbyte列を格納しない。CompactMetaと必ず併用、Eytzingerとは併用しない |

Rust readerは、未知のビット、OmittedKeysとCompactMetaの不整合、OmittedKeysとEytzingerの併用、CompactMetaなのにページ長が32767を超える場合を、いずれもエラーにする。

metadataの4形式は次のとおり。offsetはページ先頭からの絶対位置。

| 形式 | leaf | internal |
| --- | --- | --- |
| classic | 1 entryにつき `offset(i32) key_len(u16) value_len(u16)` | 1 entryにつき `offset(i32) key_len(u16)` |
| compact | `u16 offset[n+1]` と、キーを持つなら `u16 key_len[n]` | `u16 offset[n+1]` |
| compact + OmittedKeys | `u16 offset[n+1]`（key_len配列なし） | metadata領域そのものが無く、payloadが8 bytesの子ordinal配列 |

leafのvalue長が `0xFFFF` のとき、またはcompact形式でoffsetのbit 15が立っているとき、そのentryのinline payloadは値ではなく8 bytesのblob page ordinalになる。blob pageはentry_countが0のleafで、payloadは28 bytes目から値そのものが入る。

### digestと検索

全ページのdigest配列は順序を保つ64-bit値で、検索はまずこれを見る。同じdigestが並ぶ区間だけをキーで比較する。OmittedKeysのページはキーのbyte列がないので、比較そのものがdigestの比較になる。

| encoding ID | キー | 比較 | digest |
| --- | --- | --- | --- |
| `i64` | 8 bytes LE | 符号付き整数 | 符号bitを反転した値。全単射なのでOmittedKeysの対象になる |
| `ascii` | 任意長 | byte辞書順 | 先頭8 bytesをbig-endianで詰め、足りない分は0埋め |
| `uuidv7` | 16 bytes（.NET `Guid` のbyte順） | §4 | 先頭3 fieldを並べた値 |
| `ulid` | 16 bytes | byte辞書順 | 先頭8 bytesをbig-endianで読む |

### 副索引

副索引のツリーは値の位置に16 bytesの`PageRef`を持つ。[U8]

```text
0  8  page ordinal (i64)
8  4  対象ページ内の開始位置 (i32)
12 4  長さ (i32)
```

非ユニーク副索引は、同じキーを区別するためにキーの末尾へ4 bytesのrecord id（i32 LE）を足した複合キーをツリーに格納する。record idは索引キーごとに0から順に振られ、同じ索引キーの中では主キー順になる。C# builderも、主キー順に走査しながら副索引の行を作るので同じ順序になる。

## 4. 実測で確定した事項

推測ではなく、.NET 10上で実行して確認した項目を記録する。

**`Guid.CompareTo` の比較は符号なし。** `uuidv7` encodingの比較はC#の `Guid.CompareTo` に従う。先頭3 field（offset 0のu32、offset 4のu16、offset 6のu16）を符号なしで比較し、その後は8 byte目以降をbyte辞書順で比較する。`_a` が `0x7f000000` と `0x80000000` のGuidを比較すると `-1` が返る。符号付き比較なら `1` になるので、符号なしで確定する。

**`Guid.TryWriteBytes` は.NETのbyte順で書く。** `01890a5d-ac96-774b-bcce-b302099a8057` を書き出すと `5d0a890196ac4b77bcceb302099a8057` になる。先頭3 fieldがlittle-endian、残り8 bytesはそのままの順。RFC 4122のbyte順とは違うので、Rust側には `Uuidv7Encoding::from_rfc4122` と `to_rfc4122` を用意した。

**`Ulid` の比較はbyte辞書順。** Cysharp/Ulid 1.3.4で、先頭byteが `0x7f` と `0x80` のUlid、末尾byteが `0x7f` と `0x80` のUlidを比較して、どちらも `-1` を確認した。

**zstd frameは双方向に読める。** C#側の `NativeCompressions` が書いたframeをRust側の `zstd-safe` が読み、その逆も通る。page filterを有効にしたfixtureは、両実装の出力がbyte単位で一致した。

## 5. 対応範囲

| 項目 | 状況 |
| --- | --- |
| 1.4、無圧縮、複数table、raw value、i64 / ascii | 双方向で確認済み |
| classic / compact / OmittedKeys / Eytzingerの各構成 | 双方向で確認済み |
| range / prefix / count / cursor / 昇降順 | 双方向で確認済み。prefixの扱いは§6のD4 |
| unique / 非unique副索引、overflow | 双方向で確認済み。非uniqueの例外は§6のD1、D2、D8 |
| UUIDv7 / ULID | 双方向で確認済み |
| `DryDB.ZstdCompression` | 双方向で確認済み |
| MessagePack値 | fixture DTO 1件について双方向で確認済み。配列形式とmap形式の両方 |
| rkyv値 | raw bytesとしては互換。型付きの読み出しはRust側の追加機能で、C#からはできない |
| page filterを2つ以上宣言したファイル | 非対応。§6のD3 |
| C#利用者の任意custom filter / encoding | 同じIDと意味論のRust実装を登録した場合のみ |
| Unity固有loader / IL2CPP統合 | 対象外 |
| 1.0〜1.3、および将来の版 | 非対応。versionが違えばエラー |

MessagePackの互換はDTO単位で成立する。フィールドの順序（配列形式の場合）または名前（map形式の場合）、整数の幅、null許容の扱いが両側で一致していることが条件になる。任意のDTOが往復することは主張しない。確認したDTOの定義は `crates/drydb-interop/tests/msgpack.rs` と `tests/interop/DryDbOracle/MessagePackFixture.cs` にある。

## 6. 例外台帳

upstreamの動作をテストで実測し、それに合わせなかった項目を記録する。各項目に対応するテストが `crates/drydb-interop/tests/interop.rs` にあり、upstream側の挙動を文章ではなくassertionとして固定している。upstreamかこちらの挙動が変われば、テストが落ちる。

### D1. 非ユニーク副索引で、索引キーが8 bytes未満のとき

upstreamのbuilderは、非ユニーク副索引のページに書くdigestを「複合キー全体」から計算する。一方upstreamのreaderは、検索キーのdigestを「複合キーのうちsource key部分だけ」から計算する。[U5][U9] この2つは、source keyが既にdigestの8 bytesを埋めている場合にのみ一致する。

結果として、C# builderが生成したファイルに対し、3 bytesの索引キーで30件の該当行を引くと、C# readerはrecord idが0の1件しか返さない。`i64`（8 bytes固定）、`uuidv7` と `ulid`（先頭8 bytesだけを見る）では、この問題は起きない。`ascii` で短いキーを使ったときだけ現れる。

この実装は、readerと同じ規則、つまりsource keyだけからdigestを計算して書く。そのため生成したファイルは、キー長によらずC# readerからも全件引ける。C# builderが生成したファイルを読むときは、digestが一致しなくても該当行を取りこぼさない検索方法を使うので、やはり全件返す。ページのdigest検査（§7）も、非ユニーク副索引についてはこの2つの規則のどちらも受け入れる。

テスト: `divergence_short_non_unique_index_keys`。索引キー3 bytesでC#が1件、こちらが30件、こちらが書いたファイルならC#も30件返すことを確認している。`a_non_unique_index_whose_digests_fall_at_a_carry_is_readable` は、record idが255から256へ変わってdigestが減るファイルを読めることと、検証を通ることを確認している。

### D2. 非ユニーク副索引の排他境界

upstreamは索引キー `k` に対する排他下限を、複合キー `(k, 0)` の排他境界として扱う。record idが1以上の行は残るので、「k より大きいもの」を求めたつもりでも、kの行がほぼすべて返る。

この実装は、排他下限を `(k, i32::MAX)` の排他境界、排他上限を `(k, 0)` の排他境界に対応させる。索引キー単位で境界が閉じる。両端が同じキーで少なくとも片方が排他の場合は、この変換の前に空の範囲として扱う。変換してしまうと下限が上限を追い越し、呼び出し側の指定ミス（逆転した範囲）と区別がつかなくなる。主キーへの同じ問い合わせは0件を返す。record idは0以上でなければならない。負の値は `(k, 0)` からの範囲の下に落ちるので、その行はページにも件数にも現れるのに、どの検索からも見えなくなる。

テスト: `divergence_exclusive_bounds_on_a_non_unique_index`。同じファイルに同じ問い合わせをして、C#が29件、こちらが15件を返すことを確認している。

### D3. page filterを2つ以上宣言したファイル

upstreamのencoderは、filterが2つ以上あるとき、最後に書き出すbufferを取り違えて1番目のfilterの出力だけを書く。decoderは1番目に `Decode`、2番目以降に `Encode` を呼ぶ。[U3] どちらの経路も、2つ以上のfilterを通したファイルを正しく扱えない。

この実装は、有効なファイルが存在しない合成順序を推測するより、filterが2つ以上宣言されたファイルを `Unsupported` として拒否する。1つまでのfilterは双方向で動作する。

### D4. 空のキーとprefix

C#のAPIは範囲の端を空のbyte列で表すので、「空のキー」と「無限端」を区別できない。この実装は `Bound` で無限端を明示し、空のbyte列は長さ0のキーとして扱う。

prefix検索は、キーの順序がbyte辞書順と一致するencoding（`ascii` と `ulid`）に限って受け付ける。`i64` や `uuidv7` はbyte順とキー順が一致しないので、byte prefixは範囲を表さない。upstreamは encoding を問わずbyte prefixで範囲を作るため、`i64` に対しては意味のない結果を返す。この実装はその場合 `InvalidArgument` を返す。

空のprefixは、upstreamでは常に空の結果になる。この実装は全件を返す。

prefixはキーそのものではないので、固定長encodingの幅の検査は掛けない。`ulid` の16 byteに対して先頭1 byteのprefixを与えるのは正しい使い方で、CLIの `drydb prefix` もこれを受け付ける。`get` に短いキーを与えた場合は、これまでどおり幅の不一致として拒否する。

### D5. 行が1件もないtable

upstreamのbuilderは、行のないtableにページを1枚も書かず、root ordinalに `-1` を記録する。そのファイルを自分のreaderで開くと、page directoryを `-1` で添字参照して例外になる。さらに、databaseのtableがすべて空でページが1枚もない場合、builder自体が例外で失敗する。

この実装は、空のtableにも空のleaf pageを1枚書く。生成したファイルはC# readerからも開けて、0件と読める。C# builderが生成した `-1` のファイルを読むときは、空のtableとして扱う。

テスト: `divergence_empty_table_root`。

### D6. inline値の長さが0xFFFF以上のとき

classic metadataでは値の長さをu16に格納し、`0xFFFF` はoverflowのsentinelとして予約されている。upstreamのbuilderは長さがちょうど `0xFFFF` の値だけをoverflowへ回すので、page sizeが大きいときに `0xFFFF` を超える値をinlineに書いてしまい、長さが切り詰められる。page size 200000で100000 bytesの値を書くと、読み出しは34464 bytesになる。

この実装は、長さが `0xFFFF` 以上の値をすべてblob pageへ回す。生成したファイルではC# readerも値を完全に読み出せる。C# builderが生成したファイルを読むときは、ファイルに書かれているとおり切り詰められた値を返す。ファイルの内容を作り変えることはしない。

テスト: `divergence_inline_value_length_overflow`。

### D7. 1ページに2つのseparatorが入らないとき

page sizeが小さく、キーが長いと、内部ページに separator が1つしか入らない構成になる。この状態では、1つの段が受け取った件数と同じだけの separator を上の段へ渡すので、木の段数だけが増えて件数が減らない。

upstreamのbuilderはこの状態を検査しない。page size 128、Eytzinger digest、29 bytesと33 bytesのasciiキー2件で実行したところ、45秒経っても終わらず、出力ファイルは527 MBまで増えていた。

この実装は、キーをappendした時点で「内部ページにこの長さのseparatorが2つ入るか」を検査し、入らなければ `ValueTooLarge` を返す。同じキーを2つ並べる前提の検査なので、短いキーと組み合わせれば収まる場合も拒否する。page sizeが最小値に近いときだけ効く。

テスト: `divergence_a_page_too_small_to_hold_two_separators`。upstreamが制限時間内に終わらないことと、こちらがエラーを返すことを確認している。

### D8. 同じ索引キーの行が複数の葉にまたがるとき

キーのbyte列を持たない内部ページでは、キーの比較がdigestの比較になる。同じ索引キーを持つ行が複数の葉にまたがると、それらの葉のseparatorはすべて同じに見える。upstreamの降下は、比較が等しい子を最後まで通り過ぎて末尾の子へ入るので、手前の葉に入った行へは到達しない。

upstream builderで、page size 256、40行すべてに同じi64の副索引キーを付けたファイルを作り、C# readerで引くと40件中の一部しか返らない。

該当する行が葉の途中から始まる場合も同じ形で落ちる。page size 256、主キー `0..39`、最初の5行の副索引キーを0、残り35行を1にしたファイルでは、キー1の行は最初の葉の途中から始まる。C# readerは35件のうち4件しか返さない。比較が等しい最初のseparatorの子に入っても、その手前の子の末尾にある該当行には届かない。

この実装は、separatorがキーより厳密に小さい最後の子へ降下し、そこから葉の右兄弟をたどって範囲の終わりまで進む。どちらのファイルからも全件を返す。葉をまたぐ走査はファイルのページ数で上限を置いてあるので、兄弟の鎖が循環していれば止まる。

テスト: `divergence_duplicate_index_keys_spanning_leaves` と `divergence_index_key_starting_inside_a_leaf`。

### D9. 非ユニーク副索引のページレイアウト

upstreamは、非ユニーク副索引のページでキーのbyte列を省略するかどうかを、source encodingのdigestが全単射かどうかで決める。`i64` を source encoding にすると省略する側に倒れるが、実際にツリーへ格納されている複合キーにはrecord idが付いており、digestはそれを含まない。同じ索引キーを持つ行がページ上で区別できなくなる。

この実装は、非ユニーク副索引のページには必ずキーのbyte列を書く。upstreamが省略して書いたページも読めるが、その場合record idは復元できない。索引のcursorはsource keyだけを返すので、公開APIから見える結果は変わらない。

この違いがあるため、非ユニーク副索引を `i64` で作ったfixtureでは、両実装の出力がbyte単位では一致しない。読み出した結果は一致する。

なお、この実装のbuilderは木の段数に64段の上限を置いてある。1ページに2つのseparatorが入る構成なら段数は対数的にしか増えないので到達しないが、収束しない状態に入った場合はそこで止まる。

## 7. 安全性に関する方針

有効なファイルと意図された検索意味論の互換を目標とする。未定義動作、解放済みメモリの参照、入力長を無視した読み取りは移植しない。

upstreamのreaderは、ファイルのbyte列を `Unsafe.ReadUnaligned` で構造体として読み、範囲検査をしない。この実装はすべてのフィールドを明示的にlittle-endianで復号し、長さを検査したslice越しに読む。破損したファイルからは `CorruptData` が返り、panicや未定義動作にはならない。`crates/drydb/tests/corrupt.rs` が、有効なファイルの1 byteを書き換えたものを多数作って全読み取り経路を実行し、これを確認している。

ページの並びも検査する（`OpenOptions::validate_digests`、既定で有効）。各digestがそのキーのdigestと一致すること、キーが厳密な昇順であることを見る。キーのbyte列を持たないページでは、代わりにdigestが昇順であることを見る。検索はdigest配列で比較開始位置を決め、そこから目的のキーより大きいキーに当たるまで前へ進むので、どちらが崩れても、どの境界も踏み外さないまま結果だけが欠ける。この検査が無いと、そういうファイルは「エラー」ではなく「行が足りない答え」を返す。

キーがあるページでdigestの昇順は見ない。D1のとおり、C# builderは非ユニーク副索引のdigestをrecord idまで含めて計算する。record idはlittle-endianなので、255から256へ変わるところでdigestが減る。digestは順序を保つ関数なので、キーが昇順で各digestがそのキーと一致していれば、こちらの規則で書いたファイルのdigestは自動的に昇順になる。検査したかどうかはページのbufferに記録するので、blobの読み出しや `verify` が先に同じページをcacheへ入れていても、木がそれを使う前に検査が走る。Eytzinger配置のページでは、完全二分木のBFS順を走査してentryと対応付け、entryを超えた部分が `u64::MaxValue` であることも確かめる。非ユニーク副索引については§6のD1にある2つの規則のどちらも受け入れる。対象外になるのはキーのbyte列を持たないページだけで、そこではdigestがキー列そのものになる。

upstreamの `NonUniqueSecondaryIndexQuery` は、範囲の端が空のとき4 bytesのbufferに対して source encoding の比較を呼ぶ。`i64` の場合、長さ0のspanから8 bytesを読む。相互運用テストのfixtureはこの経路を踏まないよう作ってある。

## 8. 一次資料

以下のupstreamリンクはすべて同一commitを指す。

- [U1: 対象commit](https://github.com/hadashiA/DryDB/commit/6b175929491793948e63430c20c2d6f58300d97f)
- [U2: DryDBCodec.Decode.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/DryDBCodec.Decode.cs)
- [U3: DryDBCodec.Encode.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/DryDBCodec.Encode.cs)
- [U4: BTree/NodeHeader.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/BTree/NodeHeader.cs)
- [U5: BTree/TreeBuilder.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/BTree/TreeBuilder.cs)
- [U6: IKeyEncoding.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/IKeyEncoding.cs)
- [U7: Internal/PageCache.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/Internal/PageCache.cs)
- [U8: Internal/PageRef.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/Internal/PageRef.cs)
- [U9: Internal/KeyComparers.cs](https://github.com/hadashiA/DryDB/blob/6b175929491793948e63430c20c2d6f58300d97f/src/DryDB/Internal/KeyComparers.cs)

本書内の`[U番号]`はこの一覧を指す。upstreamのREADMEに載っている速度値は、この実装の性能根拠には使わない。性能の測り方は[ベンチマーク](benchmarks.md)を参照。
