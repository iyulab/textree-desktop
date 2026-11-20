using LMSupply.Embedder;
using Textree.Host;
using Textree.Host.Rag;
using Xunit;

[Trait("Category", "Integration")]
public class VaultManagerScopeTests
{
    private static async Task<VaultManager> NewManagerAsync()
    {
        var opts = new TextreeHostOptions
        {
            IndexRoot = Path.Combine(Path.GetTempPath(), "textree-test", Guid.NewGuid().ToString("n"))
        };
        // CPU here on purpose: results must not depend on which GPU the machine running the
        // tests has. The product lets the runtime pick (ExecutionProvider.Auto).
        var model = await LocalEmbedder.LoadAsync(
            opts.EmbeddingModel,
            new EmbedderOptions { Provider = LMSupply.ExecutionProvider.Cpu });
        var slot = new EmbedderSlot();
        slot.Set(model);
        return new VaultManager(slot, opts);
    }

    [Fact]
    public async Task SubScopeExcludesRootNotes()
    {
        using var mgr = await NewManagerAsync();
        var vault = Path.GetFullPath("fixtures");
        await mgr.ReindexAsync(vault, default);

        var sub = await mgr.SearchAsync(vault, "payment refund", Path.Combine(vault, "sub"), 10, default);
        Assert.NotEmpty(sub);
        // SearchAsync returns vault-relative POSIX paths (e.g. "sub/sub-note.md"), so the
        // separator is always '/' regardless of host OS. Assert on POSIX sub-path prefix.
        Assert.All(sub, h => Assert.StartsWith("sub/", h.SourcePath));

        var root = await mgr.SearchAsync(vault, "payment refund", vault, 10, default);
        Assert.Contains(root, h => h.SourcePath.Contains("sub-note"));
        // North star: same query, scope only differs -> result set differs (root sees >= sub).
        var rootFiles = root.Select(h => h.SourcePath).Distinct().Count();
        var subFiles = sub.Select(h => h.SourcePath).Distinct().Count();
        Assert.True(rootFiles >= subFiles);
    }

    [Fact]
    public async Task ReindexIsIdempotent_NoDuplicateAccumulation()
    {
        using var mgr = await NewManagerAsync();
        var vault = Path.GetFullPath("fixtures");

        await mgr.ReindexAsync(vault, default);
        var after1 = await mgr.SearchAsync(vault, "payment refund", vault, 50, default);

        await mgr.ReindexAsync(vault, default);   // reindex the SAME vault again
        var after2 = await mgr.SearchAsync(vault, "payment refund", vault, 50, default);

        // Re-indexing the same unchanged content must not grow the index (idempotent upsert).
        Assert.Equal(after1.Count, after2.Count);
    }

    [Fact]
    public async Task RememorizingAnEditedNoteReplacesItsOldPassages()
    {
        using var mgr = await NewManagerAsync();
        var vault = Path.Combine(Path.GetTempPath(), "textree-test", Guid.NewGuid().ToString("n"));
        Directory.CreateDirectory(vault);
        var note = Path.Combine(vault, "note.md");
        try
        {
            await File.WriteAllTextAsync(note, "# Migration\n\nThe zebra herd crosses the river every winter.\n");
            await mgr.MemorizeAsync(vault, note, default);
            var before = await mgr.SearchAsync(vault, "zebra herd river", vault, 10, default);
            Assert.Contains(before, h => h.Snippet.Contains("zebra"));

            await File.WriteAllTextAsync(note, "# Lighthouse\n\nThe keeper trims the lamp before the storm.\n");
            await mgr.MemorizeAsync(vault, note, default);

            // The file is indexed once, under its new content only: the old passage is gone
            // rather than left beside the new one.
            var after = await mgr.SearchAsync(vault, "zebra herd river", vault, 10, default);
            Assert.NotEmpty(after);
            Assert.All(after, h => Assert.DoesNotContain("zebra", h.Snippet));
            Assert.Contains(after, h => h.Snippet.Contains("keeper"));
        }
        finally
        {
            mgr.Dispose();
            try { Directory.Delete(vault, recursive: true); } catch (IOException) { }
        }
    }
}

[Trait("Category", "Integration")]
public class VaultManagerForgetTests
{
    private static async Task<(VaultManager mgr, string root)> NewManagerAsync()
    {
        var root = Path.Combine(Path.GetTempPath(), "textree-test", Guid.NewGuid().ToString("n"));
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
        var dir = Directory.CreateTempSubdirectory("forget-vault-").FullName;
        File.WriteAllText(Path.Combine(dir, "note.md"), note);
        return dir;
    }

    private static string[] Kept(string root, string vault)
    {
        var hash = VaultHash.For(vault);
        return Directory.Exists(root)
            ? Directory.EnumerateFileSystemEntries(root).Where(p => Path.GetFileName(p).StartsWith(hash)).ToArray()
            : [];
    }

    [Fact]
    public async Task Forgetting_a_folder_removes_what_was_indexed_from_it_and_nothing_else()
    {
        var (mgr, root) = await NewManagerAsync();
        using var _ = mgr;
        var kept = Vault("refunds are paid within thirty days");
        var other = Vault("the garden needs water every morning");
        await mgr.ReindexAsync(other);
        await mgr.ReindexAsync(kept); // the folder in use when it is forgotten
        Assert.NotEmpty(Kept(root, kept));

        Assert.True(await mgr.ForgetAsync(kept));

        Assert.Empty(Kept(root, kept));
        Assert.NotEmpty(Kept(root, other));
        var hits = await mgr.SearchAsync(other, "garden water", other, 5);
        Assert.NotEmpty(hits);
    }

    [Fact]
    public async Task A_forgotten_folder_can_be_indexed_again()
    {
        var (mgr, root) = await NewManagerAsync();
        using var _ = mgr;
        var vault = Vault("refunds are paid within thirty days");
        await mgr.ReindexAsync(vault);
        await mgr.ForgetAsync(vault);

        await mgr.ReindexAsync(vault);

        Assert.NotEmpty(await mgr.SearchAsync(vault, "refund", vault, 5));
    }

    [Fact]
    public async Task Forgetting_a_folder_that_was_never_indexed_is_fine()
    {
        var (mgr, root) = await NewManagerAsync();
        using var _ = mgr;
        Assert.True(await mgr.ForgetAsync(Path.Combine(root, "never")));
    }

    [Fact]
    public async Task A_reindex_running_when_its_folder_is_forgotten_stops_and_leaves_nothing()
    {
        var (mgr, root) = await NewManagerAsync();
        using var _ = mgr;
        var vault = Directory.CreateTempSubdirectory("forget-vault-").FullName;
        for (var i = 0; i < 40; i++)
            File.WriteAllText(Path.Combine(vault, $"n{i}.md"), $"note {i} about refunds and payments number {i}");

        var running = mgr.ReindexAsync(vault);
        while (Kept(root, vault).Length == 0 && !running.IsCompleted) await Task.Delay(10);
        await mgr.ForgetAsync(vault);

        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => running);
        Assert.Empty(Kept(root, vault)); // one call, as the app makes it

    }
}
