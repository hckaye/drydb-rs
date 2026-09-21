// Oracle for the interop tests: builds and reads DryDB files with the pinned upstream
// C# implementation so the Rust implementation can be compared against it.
//
// Both sides are driven by the same fixture spec, so "C# built it, Rust read it" and
// "Rust built it, C# read it" exercise the same data and the same queries.
//
//   DryDbOracle build <spec.json> <out.drydb>
//   DryDbOracle dump  <spec.json> <db.drydb> <out.json>

using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using DryDB;
using DryDB.Compression;
using DryDB.UlidKey;

namespace DryDbOracle;

static class Program
{
    static int Main(string[] args)
    {
        // Touching the instance runs the static constructor that registers `ulid`.
        _ = UlidKeyEncoding.Instance;
        // The zstd filter registers itself from its static constructor, which only runs
        // when the type is first used; `dump` has to resolve it without ever building.
        RuntimeHelpers.RunClassConstructor(typeof(ZstdCompressionPageFilter).TypeHandle);

        if (args.Length < 1)
        {
            Console.Error.WriteLine("usage: DryDbOracle build|dump ...");
            return 2;
        }
        try
        {
            switch (args[0])
            {
                case "build" when args.Length == 3:
                    BuildAsync(args[1], args[2]).GetAwaiter().GetResult();
                    return 0;
                case "dump" when args.Length == 4:
                    DumpAsync(args[1], args[2], args[3]).GetAwaiter().GetResult();
                    return 0;
                case "msgpack-build" when args.Length == 3:
                    MessagePackFixture.BuildAsync(args[1], args[2]).GetAwaiter().GetResult();
                    return 0;
                case "msgpack-dump" when args.Length == 4:
                    MessagePackFixture.DumpAsync(args[1], args[2], args[3]).GetAwaiter().GetResult();
                    return 0;
                default:
                    Console.Error.WriteLine(
                        "usage: DryDbOracle build <spec> <out> | dump <spec> <db> <out>"
                        + " | msgpack-build <layout> <out> | msgpack-dump <layout> <db> <out>");
                    return 2;
            }
        }
        catch (Exception e)
        {
            Console.Error.WriteLine(e.ToString());
            return 1;
        }
    }

    static IKeyEncoding EncodingFor(string id) => KeyEncoding.FromId(id);

    static byte[] B64(JsonElement e) => Convert.FromBase64String(e.GetString()!);

    static byte[] DeriveIndexKey(string rule, ReadOnlyMemory<byte> key, ReadOnlyMemory<byte> value)
    {
        if (rule == "key")
        {
            return key.ToArray();
        }
        if (rule.StartsWith("value_prefix:", StringComparison.Ordinal))
        {
            var n = int.Parse(rule["value_prefix:".Length..]);
            return value.Span[..Math.Min(n, value.Length)].ToArray();
        }
        if (rule.StartsWith("value_suffix:", StringComparison.Ordinal))
        {
            var n = int.Parse(rule["value_suffix:".Length..]);
            var take = Math.Min(n, value.Length);
            return value.Span[(value.Length - take)..].ToArray();
        }
        if (rule.StartsWith("const:", StringComparison.Ordinal))
        {
            return Convert.FromBase64String(rule["const:".Length..]);
        }
        throw new InvalidOperationException($"unknown index key rule `{rule}`");
    }

    static async Task BuildAsync(string specPath, string outPath)
    {
        using var spec = JsonDocument.Parse(File.ReadAllBytes(specPath));
        var root = spec.RootElement;

        using var builder = new DatabaseBuilder
        {
            PageSize = root.GetProperty("pageSize").GetInt32(),
            EytzingerDigests = root.TryGetProperty("eytzinger", out var e) && e.GetBoolean(),
        };

        if (root.TryGetProperty("filter", out var filter) && filter.ValueKind != JsonValueKind.Null)
        {
            switch (filter.GetString())
            {
                case "zstd":
                    builder.AddPageFilter(options => options.AddZstandardCompression());
                    break;
                case null:
                    break;
                default:
                    throw new InvalidOperationException($"unknown filter `{filter.GetString()}`");
            }
        }

        foreach (var table in root.GetProperty("tables").EnumerateArray())
        {
            var tableBuilder = builder.CreateTable(
                table.GetProperty("name").GetString()!,
                EncodingFor(table.GetProperty("encoding").GetString()!));

            if (table.TryGetProperty("indexes", out var indexes))
            {
                foreach (var index in indexes.EnumerateArray())
                {
                    var rule = index.GetProperty("keyFrom").GetString()!;
                    tableBuilder.AddSecondaryIndex(
                        index.GetProperty("name").GetString()!,
                        index.GetProperty("unique").GetBoolean(),
                        EncodingFor(index.GetProperty("encoding").GetString()!),
                        (key, value) => DeriveIndexKey(rule, key, value));
                }
            }

            foreach (var row in table.GetProperty("rows").EnumerateArray())
            {
                tableBuilder.Append(B64(row.GetProperty("k")), B64(row.GetProperty("v")));
            }
        }

        if (File.Exists(outPath))
        {
            File.Delete(outPath);
        }
        await builder.BuildToFileAsync(outPath);
    }

    static async Task DumpAsync(string specPath, string dbPath, string outPath)
    {
        using var spec = JsonDocument.Parse(File.ReadAllBytes(specPath));
        var root = spec.RootElement;

        using var db = await ReadOnlyDatabase.OpenFileAsync(dbPath);
        await using var stream = File.Create(outPath);
        using var writer = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = false });

        writer.WriteStartObject();

        writer.WritePropertyName("tables");
        writer.WriteStartArray();
        foreach (var tableSpec in root.GetProperty("tables").EnumerateArray())
        {
            var name = tableSpec.GetProperty("name").GetString()!;
            var table = db.GetTable(name);

            writer.WriteStartObject();
            writer.WriteString("name", name);

            writer.WritePropertyName("scan");
            writer.WriteStartArray();
            using (var iterator = table.CreateIterator(IteratorDirection.Forward))
            {
                while (iterator.MoveNext())
                {
                    writer.WriteStartObject();
                    writer.WriteBase64String("k", iterator.CurrentKey.Span);
                    writer.WriteBase64String("v", iterator.CurrentValue.Span);
                    writer.WriteEndObject();
                }
            }
            writer.WriteEndArray();

            writer.WritePropertyName("scanDescending");
            writer.WriteStartArray();
            using (var iterator = table.CreateIterator(IteratorDirection.Backward))
            {
                while (iterator.MoveNext())
                {
                    writer.WriteStartObject();
                    writer.WriteBase64String("k", iterator.CurrentKey.Span);
                    writer.WriteBase64String("v", iterator.CurrentValue.Span);
                    writer.WriteEndObject();
                }
            }
            writer.WriteEndArray();

            writer.WriteNumber("count", table.CountRange(KeyRange.Unbound, KeyRange.Unbound));
            writer.WriteEndObject();
        }
        writer.WriteEndArray();

        var queries = root.GetProperty("queries");

        writer.WritePropertyName("points");
        writer.WriteStartArray();
        foreach (var query in queries.GetProperty("points").EnumerateArray())
        {
            var table = db.GetTable(query.GetProperty("table").GetString()!);
            using var result = table.Get(B64(query.GetProperty("key")));
            writer.WriteStartObject();
            writer.WriteBoolean("found", result.HasValue);
            if (result.HasValue)
            {
                writer.WriteBase64String("value", result.Value.Span);
            }
            writer.WriteEndObject();
        }
        writer.WriteEndArray();

        writer.WritePropertyName("ranges");
        writer.WriteStartArray();
        foreach (var query in queries.GetProperty("ranges").EnumerateArray())
        {
            var table = db.GetTable(query.GetProperty("table").GetString()!);
            var lower = OptionalKey(query, "lower");
            var upper = OptionalKey(query, "upper");
            var order = query.GetProperty("order").GetString() == "desc"
                ? SortOrder.Descending
                : SortOrder.Ascending;
            using var result = table.GetRange(
                lower,
                upper,
                query.GetProperty("lowerExclusive").GetBoolean(),
                query.GetProperty("upperExclusive").GetBoolean(),
                order);
            writer.WriteStartArray();
            foreach (var value in result)
            {
                writer.WriteBase64StringValue(value.Span);
            }
            writer.WriteEndArray();
        }
        writer.WriteEndArray();

        writer.WritePropertyName("counts");
        writer.WriteStartArray();
        foreach (var query in queries.GetProperty("counts").EnumerateArray())
        {
            var table = db.GetTable(query.GetProperty("table").GetString()!);
            writer.WriteNumberValue(table.CountRange(
                OptionalKey(query, "lower"),
                OptionalKey(query, "upper"),
                query.GetProperty("lowerExclusive").GetBoolean(),
                query.GetProperty("upperExclusive").GetBoolean()));
        }
        writer.WriteEndArray();

        writer.WritePropertyName("indexLookups");
        writer.WriteStartArray();
        foreach (var query in queries.GetProperty("indexLookups").EnumerateArray())
        {
            var table = db.GetTable(query.GetProperty("table").GetString()!);
            var index = table.Index(query.GetProperty("index").GetString()!);
            var key = B64(query.GetProperty("key"));
            using var result = index.GetRange(key, key);
            writer.WriteStartArray();
            foreach (var value in result)
            {
                writer.WriteBase64StringValue(value.Span);
            }
            writer.WriteEndArray();
        }
        writer.WriteEndArray();

        writer.WritePropertyName("indexRanges");
        writer.WriteStartArray();
        foreach (var query in queries.GetProperty("indexRanges").EnumerateArray())
        {
            var table = db.GetTable(query.GetProperty("table").GetString()!);
            var index = table.Index(query.GetProperty("index").GetString()!);
            var order = query.GetProperty("order").GetString() == "desc"
                ? SortOrder.Descending
                : SortOrder.Ascending;
            using var result = index.GetRange(
                OptionalKey(query, "lower"),
                OptionalKey(query, "upper"),
                query.GetProperty("lowerExclusive").GetBoolean(),
                query.GetProperty("upperExclusive").GetBoolean(),
                order);
            writer.WriteStartArray();
            foreach (var value in result)
            {
                writer.WriteBase64StringValue(value.Span);
            }
            writer.WriteEndArray();
        }
        writer.WriteEndArray();

        writer.WriteEndObject();
        writer.Flush();
    }

    static byte[] OptionalKey(JsonElement query, string name)
    {
        if (!query.TryGetProperty(name, out var value) || value.ValueKind == JsonValueKind.Null)
        {
            return KeyRange.Unbound;
        }
        return Convert.FromBase64String(value.GetString()!);
    }
}
