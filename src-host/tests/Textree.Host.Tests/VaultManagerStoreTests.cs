using LMSupply.Embedder;
using Microsoft.Data.Sqlite;
using Textree.Host;
using Textree.Host.Rag;
using Xunit;

[Trait("Category", "Integration")]
public class VaultManagerStoreTests
{
    private static async Task<(VaultManager mgr, string root)> NewManagerAsync(string? root = null)
    {
        root ??= Path.Combine(Path.GetTempPath(), "textree-test", Guid.NewGuid().ToString("n"));
        var opts = new TextreeHostOptions { IndexRoot = root };
        var model = await LocalEmbedder.LoadAsync(
            opts.EmbeddingModel,
            new EmbedderOptions { Provider = LMSupply.ExecutionProvider.Cpu });
        var slot = new EmbedderSlot();
        slot.Set(model);
        return (new VaultManager(slot, opts), root);
    }

    private static string Vault(string note)
    {
        var dir = Directory.CreateTempSubdirectory("store-vault-").FullName;
        File.WriteAllText(Path.Combine(dir, "note.md"), note);
        return dir;
    }

    private static string Db(string root, string vault) => Path.Combine(root, VaultHash.For(vault) + ".db");

    private static void Execute(string db, string sql)
    {
        SqliteConnection.ClearAllPools();
        using var c = new SqliteConnection($"Data Source={db};Pooling=False");
        c.Open();
        using var cmd = c.CreateCommand();
        cmd.CommandText = sql;
        cmd.ExecuteNonQuery();
    }

    [Fact]
    public async Task An_index_made_at_another_vector_width_is_rebuilt_from_the_notes()
    {
        var (first, root) = await NewManagerAsync();
        var vault = Vault("refunds are paid within thirty days");
        await first.ReindexAsync(vault);
        first.Dispose();
        // The same model's table, declared at another width and holding vectors: what an
        // embedder that changed its width under an existing folder finds.
        Execute(Db(root, vault),
            "PRAGMA writable_schema=ON; UPDATE sqlite_master SET sql = replace(sql, 'float[384]', 'float[8]') " +
            "WHERE sql LIKE '%USING vec0%'; PRAGMA writable_schema=OFF;");

        var (mgr, _) = await NewManagerAsync(root);
        using var _ = mgr;
        await mgr.ReindexAsync(vault);

        Assert.NotEmpty(await mgr.SearchAsync(vault, "refund", vault, 5));
    }

    [Fact]
    public async Task A_search_on_an_index_that_cannot_be_read_fails_instead_of_finding_nothing()
    {
        var (first, root) = await NewManagerAsync();
        var vault = Vault("refunds are paid within thirty days");
        await first.ReindexAsync(vault);
        first.Dispose();
        Execute(Db(root, vault), "PRAGMA writable_schema=ON; UPDATE sqlite_master SET sql = replace(sql, 'USING vec0', 'USING vec0_missing') " +
            "WHERE sql LIKE '%USING vec0%'; PRAGMA writable_schema=OFF;");

        var (mgr, _) = await NewManagerAsync(root);
        using var _ = mgr;

        await Assert.ThrowsAnyAsync<Exception>(() => mgr.SearchAsync(vault, "refund", vault, 5));
    }

    [Fact]
    public async Task Indexing_one_folder_does_not_stop_another_being_indexed()
    {
        var (mgr, _) = await NewManagerAsync();
        using var _ = mgr;
        var a = Directory.CreateTempSubdirectory("store-vault-").FullName;
        for (var i = 0; i < 30; i++)
            File.WriteAllText(Path.Combine(a, $"n{i}.md"), $"note {i} about the lighthouse keeper and the lamp, number {i}");
        var b = Vault("refunds are paid within thirty days");

        // The sidebar opens its folders one after another; each asks for its whole index.
        var first = mgr.ReindexAsync(a);
        await mgr.ReindexAsync(b);
        await first;

        Assert.Equal(30, (await mgr.SearchAsync(a, "lighthouse keeper lamp", a, 50)).Select(h => h.SourcePath).Distinct().Count());
        Assert.NotEmpty(await mgr.SearchAsync(b, "refund", b, 5));
    }

    [Fact]
    public async Task A_search_on_an_index_of_another_width_fails_and_leaves_the_index_alone()
    {
        var (first, root) = await NewManagerAsync();
        var vault = Vault("refunds are paid within thirty days");
        await first.ReindexAsync(vault);
        first.Dispose();
        Execute(Db(root, vault),
            "PRAGMA writable_schema=ON; UPDATE sqlite_master SET sql = replace(sql, 'float[384]', 'float[8]') " +
            "WHERE sql LIKE '%USING vec0%'; PRAGMA writable_schema=OFF;");

        var (mgr, _) = await NewManagerAsync(root);
        using var _ = mgr;

        await Assert.ThrowsAsync<FluxIndex.Storage.SQLite.SQLiteVecDimensionMismatchException>(() => mgr.SearchAsync(vault, "refund", vault, 5));
        Assert.True(File.Exists(Db(root, vault)));
    }
}
