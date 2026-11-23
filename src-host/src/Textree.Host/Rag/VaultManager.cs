using FileFlux;
using FluxIndex.Core.Application.Interfaces;
using FluxFeed.Domain.Enums;
using FluxFeed.Extensions;
using FluxFeed.Interfaces;
using FluxIndex.Storage.SQLite;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using IEmbeddingService = FluxIndex.Core.Application.Interfaces.IEmbeddingService;

namespace Textree.Host.Rag;

/// <summary>
/// Keeps an index for each notes folder the app works with. The embedder is shared (loaded once);
/// each folder gets its own vector store DB (keyed by <see cref="VaultHash"/>) under
/// <c>IndexRoot/&lt;hash&gt;.db</c>, opened the first time the folder is asked about and kept open,
/// so indexing or searching one folder never closes another's store or stops work running for it.
/// </summary>
public sealed class VaultManager : IDisposable
{
    private readonly EmbedderSlot _embedder;
    private readonly TextreeHostOptions _options;
    private readonly Lock _gate = new();
    private readonly Dictionary<string, FolderStore> _stores = new(StringComparer.OrdinalIgnoreCase);
    private bool _disposed;

    public VaultManager(EmbedderSlot embedder, TextreeHostOptions options)
    {
        _embedder = embedder ?? throw new ArgumentNullException(nameof(embedder));
        _options = options ?? throw new ArgumentNullException(nameof(options));
    }

    public bool EmbedderReady => _embedder.Ready;

    public Task MemorizeAsync(string vaultPath, string filePath, CancellationToken ct = default)
        => WriteAsync(Path.GetFullPath(vaultPath), null, async store => await MemorizeIntoAsync(store.Vault, filePath, ct), ct);

    /// <summary>
    /// Searches the folder's index. A search that fails throws — it never comes back as "nothing
    /// found", which would tell the person their notes hold nothing on the subject. An index made at
    /// another vector width is not rebuilt here: a search only reads.
    /// </summary>
    public async Task<IReadOnlyList<SearchHit>> SearchAsync(
        string vaultPath, string query, string scopePath, int limit, CancellationToken ct = default)
    {
        var fullVaultPath = Path.GetFullPath(vaultPath);
        var store = await OpenAsync(fullVaultPath, rebuild: false, ct);
        var options = new VaultSearchOptions
        {
            TopK = limit,
            MinScore = 0,
            PathScope = [Path.GetFullPath(scopePath)],
            IncludeContent = true,
            IncludeMetadata = true,
        };

        var result = await store.Vault.SearchAsync(query, options, ct);
        return result.Items
            .Select(i => new SearchHit(
                // Return vault-relative POSIX path (e.g. "notes/x.md") rather than the
                // absolute Windows path stored in the vector index. The Rust/frontend layers
                // expect vault-relative POSIX paths: absolute paths break self-exclusion in
                // the Related notes panel and note navigation from semantic search hits.
                Path.GetRelativePath(fullVaultPath, i.SourcePath).Replace('\\', '/'),
                Snippet(i.Content),
                i.Score))
            .ToList();
    }

    /// <summary>
    /// Indexes every note of the folder. Stops (OperationCanceledException) when the folder's index is
    /// dropped meanwhile (<see cref="ForgetAsync"/>); work for other folders does not stop it.
    /// </summary>
    public async Task ReindexAsync(string vaultPath, CancellationToken ct = default)
    {
        var fullPath = Path.GetFullPath(vaultPath);
        var store = await OpenAsync(fullPath, rebuild: true, ct);
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(ct, store.Lifetime.Token);
        foreach (var file in Directory.EnumerateFiles(fullPath, "*.md", SearchOption.AllDirectories))
        {
            linked.Token.ThrowIfCancellationRequested();
            try
            {
                await WriteAsync(fullPath, store, s => MemorizeIntoAsync(s.Vault, file, linked.Token), linked.Token);
            }
            catch (Exception ex) when (ex is not OperationCanceledException && store.Lifetime.IsCancellationRequested)
            {
                // The store was closed under the note being indexed: the folder was dropped, not broken.
                throw new OperationCanceledException("The folder stopped being indexed.", ex, store.Lifetime.Token);
            }
        }
    }

    /// <summary>
    /// Drops what the host keeps for a folder — its vector store and FluxFeed's copies — so a folder the
    /// person keeps from AI leaves nothing derived from its notes behind. Stops work running for it first.
    /// Returns whether nothing is left (false: a file could not be removed — it is retried next time).
    /// </summary>
    public async Task<bool> ForgetAsync(string vaultPath)
    {
        var fullPath = Path.GetFullPath(vaultPath);

        // First stop work for it and let a note already being written finish — a write still under
        // way would otherwise put files back after they were removed.
        FolderStore? store;
        lock (_gate)
        {
            if (_stores.Remove(fullPath, out store))
                store.Lifetime.Cancel();
        }
        if (store is not null)
        {
            await Task.WhenAny(store.Work.Idle, Task.Delay(TimeSpan.FromSeconds(30)));
            store.Dispose();
        }
        return RemoveFiles(fullPath);
    }

    // Runs one write on the folder's store, counted so ForgetAsync can wait for it. `known` is the store
    // a reindex opened: its notes go into that store or nowhere — once it is dropped the reindex stops
    // rather than open the index again.
    private async Task WriteAsync(string fullPath, FolderStore? known, Func<FolderStore, Task> write, CancellationToken ct)
    {
        var store = known ?? await OpenAsync(fullPath, rebuild: true, ct);
        lock (_gate)
        {
            store.Lifetime.Token.ThrowIfCancellationRequested();
            store.Work.Enter();
        }
        try
        {
            await write(store);
        }
        finally
        {
            store.Work.Exit();
        }
    }

    /// <summary>
    /// The folder's store, built and opened once. Opening checks the vector table, so a table of the
    /// same model at another width that holds vectors shows here, typed
    /// (<see cref="SQLiteVecDimensionMismatchException"/>). With <paramref name="rebuild"/> (a write),
    /// that index is removed and made again: it is derived from the notes and rebuilt from them, so
    /// dropping it loses nothing — while every write into it would fail. Callers on one folder open it
    /// one at a time, so a store just made again is not removed by a second caller.
    /// </summary>
    private async Task<FolderStore> OpenAsync(string fullPath, bool rebuild, CancellationToken ct)
    {
        while (true)
        {
            var store = StoreFor(fullPath);
            if (store.Opened) return store;
            await store.Opening.WaitAsync(ct);
            try
            {
                if (store.Opened) return store;
                // Made again (or dropped) by a caller before this one: open what is there now.
                if (!IsCurrent(fullPath, store)) continue;
                try
                {
                    await store.Provider.GetRequiredService<IVectorStore>().GetDistinctDocumentCountAsync(ct);
                }
                catch (SQLiteVecDimensionMismatchException) when (rebuild)
                {
                    lock (_gate)
                    {
                        if (IsCurrent(fullPath, store))
                            _stores.Remove(fullPath);
                    }
                    store.Lifetime.Cancel();
                    store.Dispose();
                    if (!RemoveFiles(fullPath))
                        throw;
                    rebuild = false; // the store made next is new: a mismatch there is not this one
                    continue;
                }
                store.Opened = true;
                return store;
            }
            finally
            {
                store.Opening.Release();
            }
        }
    }

    private bool IsCurrent(string fullPath, FolderStore store)
    {
        lock (_gate)
            return _stores.TryGetValue(fullPath, out var current) && current == store;
    }

    // The folder's store, built if there is none yet.
    private FolderStore StoreFor(string fullPath)
    {
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (_stores.TryGetValue(fullPath, out var existing))
                return existing;
            var store = Build(fullPath);
            _stores[fullPath] = store;
            return store;
        }
    }

    private FolderStore Build(string fullPath)
    {
        // VaultHash.For owns path canonicalization (it re-runs Path.GetFullPath + casing/
        // separator normalization internally).
        var hash = VaultHash.For(fullPath);
        Directory.CreateDirectory(_options.IndexRoot);
        var dbPath = Path.Combine(_options.IndexRoot, $"{hash}.db");
        // FluxFeed's VaultBasePath is its STORAGE root (extracted/refined .md, per-entry
        // dirs), NOT the content tree. Keep it under IndexRoot — distinct from the user's
        // notes — so the store never pollutes the content folder. Memorized files are
        // referenced by absolute path, so scope filtering still targets the real note paths.
        var vaultStore = Path.Combine(_options.IndexRoot, hash);

        // Identity binds the vectors to the embedding model. The fingerprint is set on the
        // SQLite options so the vec table name is deterministic; FluxFeed binds the store to
        // the embedder's identity itself and refuses a store bound to a different embedder.
        var embedding = _embedder.Service;
        var identity = embedding.GetIdentity();

        var services = new ServiceCollection();
        services.AddLogging();

        // Shared embedder (singleton): determines the vector store dimension.
        services.TryAddSingleton<IEmbeddingService>(embedding);

        services.AddSQLiteVecVectorStore(o =>
        {
            o.DatabasePath = dbPath;
            o.VectorDimension = embedding.GetEmbeddingDimension();
            o.UseSQLiteVec = true;
            o.AutoMigrate = true;
            o.EmbeddingFingerprint = identity.Fingerprint;
        });

        services.AddFileFlux();

        services.AddFileVaultWithFluxIndex(o =>
        {
            o.VaultBasePath = vaultStore;
            // No Generic Host here (bare ServiceProvider): the memorize pipeline runs inline
            // inside MemorizeAsync, so there is no background worker that would need starting.
            o.EnableBackgroundProcessing = false;
            o.Chunking.Strategy = "Hierarchical";
            o.Chunking.MaxChunkSize = 1024;
            o.Chunking.OverlapSize = 128;
        });

        var provider = services.BuildServiceProvider();
        try
        {
            var scope = provider.CreateScope();
            return new FolderStore(provider, scope, scope.ServiceProvider.GetRequiredService<IVault>());
        }
        catch
        {
            provider.Dispose();
            throw;
        }
    }

    private static async Task MemorizeIntoAsync(IVault vault, string filePath, CancellationToken ct)
    {
        var fullPath = Path.GetFullPath(filePath);

        // Re-memorizing replaces the file's previous chunks (chunk ids derive from the path and
        // the passage), so an edited note needs no remove first and an unchanged one does not
        // accumulate duplicates. With background processing off the pipeline runs inline and the
        // returned entry is terminal — but a failure there comes back as an Error entry rather
        // than an exception, and a silent Error would let ReindexAsync call a vault "indexed"
        // while chunks are missing (project rule: never swallow failures without signal).
        var entry = await vault.MemorizeAsync(fullPath, waitForCompletion: true, ct);
        if (entry.Stage is ProcessingStage.Error)
        {
            throw new InvalidOperationException(
                $"Indexing failed for '{fullPath}': {entry.FirstError ?? entry.LastError}");
        }
    }

    // Removes the folder's index files — the vector store and FluxFeed's copies. Its store must be
    // closed already. Returns whether nothing is left.
    private bool RemoveFiles(string fullPath)
    {
        var hash = VaultHash.For(fullPath);
        var db = Path.Combine(_options.IndexRoot, $"{hash}.db");
        var store = Path.Combine(_options.IndexRoot, hash);
        // On Windows a file still open cannot be removed, and a handle can outlive the provider by a
        // moment (a reindex stopped mid-write): pooled SQLite connections, and statements released
        // only by their finalizers. Each try releases those and removes what it can; about a second in all.
        for (var attempt = 0; ; attempt++)
        {
            Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();
            GC.Collect();
            GC.WaitForPendingFinalizers();
            var gone = true;
            foreach (var file in new[] { db, db + "-wal", db + "-shm", db + "-journal" })
                if (File.Exists(file)) gone &= TryDelete(() => File.Delete(file));
            if (Directory.Exists(store))
                gone &= TryDelete(() => DeleteTree(store));
            if (gone || attempt == RemoveTries - 1) return gone;
            Thread.Sleep(RemoveWait);
        }
    }

    private const int RemoveTries = 10;
    private static readonly TimeSpan RemoveWait = TimeSpan.FromMilliseconds(100);

    // The copies keep a git repository, whose object files git makes read-only — on Windows a
    // read-only file is not removed with its folder, so each is made writable first.
    private static void DeleteTree(string dir)
    {
        foreach (var file in Directory.EnumerateFiles(dir, "*", SearchOption.AllDirectories))
        {
            var attributes = File.GetAttributes(file);
            if (attributes.HasFlag(FileAttributes.ReadOnly))
                File.SetAttributes(file, attributes & ~FileAttributes.ReadOnly);
        }
        Directory.Delete(dir, recursive: true);
    }

    private static bool TryDelete(Action delete)
    {
        try { delete(); return true; }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }

    private static string Snippet(string? content)
    {
        if (string.IsNullOrEmpty(content)) return string.Empty;
        return content.Length <= 200 ? content : content[..200];
    }

    /// <summary>
    /// Closes every folder's store (SQLite connections, file handles).
    /// The shared embedder is owned by the caller — never disposed here.
    /// </summary>
    public void Dispose()
    {
        List<FolderStore> stores;
        lock (_gate)
        {
            if (_disposed) return;
            _disposed = true;
            stores = [.. _stores.Values];
            _stores.Clear();
        }
        foreach (var store in stores)
        {
            store.Lifetime.Cancel();
            store.Dispose();
        }
    }

    // One folder's index: its own provider (vector store, FluxFeed), the writes under way in it, and
    // its lifetime — cancelled when the index is dropped, which stops work still running for it.
    private sealed class FolderStore(ServiceProvider provider, IServiceScope scope, IVault vault) : IDisposable
    {
        public ServiceProvider Provider { get; } = provider;
        public IVault Vault { get; } = vault;
        public CancellationTokenSource Lifetime { get; } = new();
        public Work Work { get; } = new();
        public SemaphoreSlim Opening { get; } = new(1, 1);
        public volatile bool Opened;

        public void Dispose()
        {
            scope.Dispose();
            Provider.Dispose();
            // Lifetime stays undisposed: work still holding the store reads it to learn it was dropped.
        }
    }
}

// Counts writes in flight; Idle completes when none are.
internal sealed class Work
{
    private readonly Lock _gate = new();
    private int _count;
    private TaskCompletionSource _idle = Done();

    public void Enter()
    {
        lock (_gate)
            if (_count++ == 0) _idle = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
    }

    public void Exit()
    {
        lock (_gate)
            if (--_count == 0) _idle.TrySetResult();
    }

    public Task Idle
    {
        get { lock (_gate) return _idle.Task; }
    }

    private static TaskCompletionSource Done()
    {
        var done = new TaskCompletionSource();
        done.SetResult();
        return done;
    }
}

public readonly record struct SearchHit(string SourcePath, string Snippet, float Score);
