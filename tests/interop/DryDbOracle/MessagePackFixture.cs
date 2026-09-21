// A fixed DTO pair for the MessagePack interop test.
//
// Two declarations of the same data, one per layout MessagePack-CSharp can write:
// integer keys produce an array, `keyAsPropertyName` produces a map keyed by property
// name. The Rust side mirrors both.

using System.Text;
using System.Text.Json;
using DryDB;
using DryDB.MessagePack;
using MessagePack;

namespace DryDbOracle;

[MessagePackObject]
public class ArrayItem
{
    [Key(0)] public long Id { get; set; }
    [Key(1)] public string Name { get; set; }
    [Key(2)] public int Level { get; set; }
    [Key(3)] public double Ratio { get; set; }
    [Key(4)] public bool Active { get; set; }
    [Key(5)] public int[] Tags { get; set; }
    [Key(6)] public byte[] Blob { get; set; }
    [Key(7)] public string Note { get; set; }
}

[MessagePackObject(keyAsPropertyName: true)]
public class MapItem
{
    public long Id { get; set; }
    public string Name { get; set; }
    public int Level { get; set; }
    public double Ratio { get; set; }
    public bool Active { get; set; }
    public int[] Tags { get; set; }
    public byte[] Blob { get; set; }
    public string Note { get; set; }
}

static class MessagePackFixture
{
    public const int RowCount = 40;

    static ArrayItem ArrayRow(int i) => new()
    {
        Id = i * 1000L - 5000,
        Name = $"item-{i:D3}",
        Level = i % 7,
        Ratio = i / 4.0,
        Active = i % 2 == 0,
        Tags = Enumerable.Range(0, i % 5).Select(t => t * 3).ToArray(),
        Blob = Enumerable.Range(0, i % 9).Select(b => (byte)(b * 7)).ToArray(),
        Note = i % 3 == 0 ? null : $"note {i}",
    };

    static MapItem MapRow(int i)
    {
        var a = ArrayRow(i);
        return new MapItem
        {
            Id = a.Id,
            Name = a.Name,
            Level = a.Level,
            Ratio = a.Ratio,
            Active = a.Active,
            Tags = a.Tags,
            Blob = a.Blob,
            Note = a.Note,
        };
    }

    public static async Task BuildAsync(string layout, string outPath)
    {
        using var builder = new DatabaseBuilder { PageSize = 4096 };
        var table = builder.CreateTable("items", KeyEncoding.Int64LittleEndian);
        for (var i = 0; i < RowCount; i++)
        {
            var key = BitConverter.GetBytes((long)i);
            var bytes = layout == "map"
                ? MessagePackSerializer.Serialize(MapRow(i))
                : MessagePackSerializer.Serialize(ArrayRow(i));
            table.Append(key, bytes);
        }
        if (File.Exists(outPath))
        {
            File.Delete(outPath);
        }
        await builder.BuildToFileAsync(outPath);
    }

    public static async Task DumpAsync(string layout, string dbPath, string outPath)
    {
        using var db = await ReadOnlyDatabase.OpenFileAsync(dbPath);
        var table = db.GetTable("items");

        await using var stream = File.Create(outPath);
        using var writer = new Utf8JsonWriter(stream);
        writer.WriteStartArray();
        for (var i = 0; i < RowCount; i++)
        {
            using var result = table.Get(BitConverter.GetBytes((long)i));
            if (!result.HasValue)
            {
                throw new InvalidOperationException($"row {i} is missing");
            }
            writer.WriteStartObject();
            if (layout == "map")
            {
                var item = MessagePackSerializer.Deserialize<MapItem>(result.Value.Memory);
                WriteItem(writer, item.Id, item.Name, item.Level, item.Ratio, item.Active,
                    item.Tags, item.Blob, item.Note);
            }
            else
            {
                var item = MessagePackSerializer.Deserialize<ArrayItem>(result.Value.Memory);
                WriteItem(writer, item.Id, item.Name, item.Level, item.Ratio, item.Active,
                    item.Tags, item.Blob, item.Note);
            }
            writer.WriteEndObject();
        }
        writer.WriteEndArray();
        writer.Flush();
    }

    static void WriteItem(
        Utf8JsonWriter writer,
        long id,
        string name,
        int level,
        double ratio,
        bool active,
        int[] tags,
        byte[] blob,
        string note)
    {
        writer.WriteNumber("id", id);
        writer.WriteString("name", name);
        writer.WriteNumber("level", level);
        writer.WriteNumber("ratio", ratio);
        writer.WriteBoolean("active", active);
        writer.WritePropertyName("tags");
        writer.WriteStartArray();
        foreach (var tag in tags)
        {
            writer.WriteNumberValue(tag);
        }
        writer.WriteEndArray();
        writer.WriteBase64String("blob", blob);
        if (note is null)
        {
            writer.WriteNull("note");
        }
        else
        {
            writer.WriteString("note", note);
        }
    }
}
