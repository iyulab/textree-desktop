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
/// Owns the single active vault. The embedder is shared (loaded once); each distinct
/// vault path gets its own vector store DB (keyed by <see cref="VaultHash"/>) under
/// <c>IndexRoot/&lt;hash&gt;.db</c>. Switching vaults rebuilds the provider for the new path.
/// </summary>
public sealed class VaultManager : IDisposable
{
    private readonly EmbedderSlot _embedder;
    private readonly TextreeHostOptions _options;
    private readonly Lock _gate = new();

    private string? _currentVaultPath;
    private ServiceProvider? _provider;
    private IServiceScope? _scope;
    private IVault? _vault;
    // Cancelled when the active vault is torn down — by a switch to another folder or by Forget — so
    // work still running for it (a reindex) stops instead of bringing its index back.
    private CancellationTokenSource _lifetime = new();
    // Writes into the active vault's index still under way — Forget waits for them before removing files.
    private Work _work = new();
    private bool _disposed;

    public VaultManager(EmbedderSlot embedder, TextreeHostOptions options)
    {
        _embedder = embedder ?? throw new ArgumentNullException(nameof(embedder));
        _options = options ?? throw new ArgumentNullException(nameof(options));
    }

    public bool EmbedderReady => _embedder.Ready;

    /// <summary>
    /// Ensures the active vault targets <paramref name="vaultPath"/>, (re)building the
    /// FluxIndex provider if the path changed. Returns the resolved vault.
    /// </summary>
    public IVault EnsureVault(string vaultPath) => EnsureVault(vaultPath, CancellationToken.None);

    // As EnsureVault, refusing once `lifetime` is cancelled: work started for a vault that has since been
    // torn down must not open (and so recreate) its index again. Checked under the same lock Forget takes.
    private IVault EnsureVault(string vaultPath, CancellationToken lifetime)
    {
        var fullPath = Path.GetFullPath(vaultPath);
        lock (_gate)
        {
            lifetime.ThrowIfCancellationRequested();
            if (_vault is not null &&
                string.Equals(_currentVaultPath, fullPath, StringComparison.OrdinalIgnoreCase))
            {
                return _vault;
            }

            // Tear down the previous vault and clear the fields BEFORE building the new one.
            // If the DI build below throws, the manager must not be left holding a disposed
            // provider/scope, nor a _currentVaultPath that would early-return a dead vault on
            // a same-path retry. New values are assigned only after a fully successful build.
            TearDown();

            // VaultHash.For owns path canonicalization (it re-runs Path.GetFullPath + casing/
            // separator normalization internally); fullPath here is already normalized purely
            // for the cache-key comparison above. The repeated GetFullPath is harmless.
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
            var scope = provider.CreateScope();
            var vault = scope.ServiceProvider.GetRequiredService<IVault>();

            _provider = provider;
            _scope = scope;
            _vault = vault;
            _currentVaultPath = fullPath;
            return vault;
        }
    }

    public Task MemorizeAsync(string vaultPath, string filePath, CancellationToken ct = default)
        => MemorizeAsync(vaultPath, filePath, CancellationToken.None, ct);

    private async Task MemorizeAsync(string vaultPath, string filePath, CancellationToken lifetime, CancellationToken ct)
    {
        IVault vault;
        Work work;
        lock (_gate)
        {
            vault = EnsureVault(vaultPath, lifetime);
            work = _work;
            work.Enter();
        }
        try
        {
            await MemorizeIntoAsync(vault, filePath, ct);
        }
        finally
        {
            work.Exit();
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

    public async Task<IReadOnlyList<SearchHit>> SearchAsync(
        string vaultPath, string query, string scopePath, int limit, CancellationToken ct = default)
    {
        var fullVaultPath = Path.GetFullPath(vaultPath);
        var vault = EnsureVault(fullVaultPath);
        var options = new VaultSearchOptions
        {
            TopK = limit,
            MinScore = 0,
            PathScope = [Path.GetFullPath(scopePath)],
            IncludeContent = true,
            IncludeMetadata = true,
        };

        var result = await vault.SearchAsync(query, options, ct);
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
    /// Indexes every note of the vault. Stops (OperationCanceledException) when the vault stops being
    /// the active one — another folder was opened, or <see cref="Forget"/> dropped it.
    /// </summary>
    public async Task ReindexAsync(string vaultPath, CancellationToken ct = default)
    {
        var fullPath = Path.GetFullPath(vaultPath);
        EnsureVault(fullPath);
        CancellationToken lifetime;
        lock (_gate) lifetime = _lifetime.Token;
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(ct, lifetime);
        foreach (var file in Directory.EnumerateFiles(fullPath, "*.md", SearchOption.AllDirectories))
        {
            linked.Token.ThrowIfCancellationRequested();
            try
            {
                await MemorizeAsync(fullPath, file, lifetime, linked.Token);
            }
            catch (Exception ex) when (ex is not OperationCanceledException && lifetime.IsCancellationRequested)
            {
                // The store was closed under the note being indexed: the vault was dropped, not broken.
                throw new OperationCanceledException("The folder stopped being indexed.", ex, lifetime);
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
        var hash = VaultHash.For(fullPath);

        // First stop work for it and let a note already being written finish — a write still under
        // way would otherwise put files back after they were removed.
        Work? draining = null;
        lock (_gate)
        {
            if (IsCurrent(fullPath))
            {
                _lifetime.Cancel();
                draining = _work;
            }
        }
        if (draining is not null)
            await Task.WhenAny(draining.Idle, Task.Delay(TimeSpan.FromSeconds(30)));

        lock (_gate)
        {
            if (IsCurrent(fullPath))
                TearDown();

            // Pooled SQLite connections keep the file open (on Windows, undeletable) after the provider is gone.
            Microsoft.Data.Sqlite.SqliteConnection.ClearAllPools();

            var db = Path.Combine(_options.IndexRoot, $"{hash}.db");
            var gone = true;
            foreach (var file in new[] { db, db + "-wal", db + "-shm", db + "-journal" })
                if (File.Exists(file)) gone &= TryDelete(() => File.Delete(file));
            var store = Path.Combine(_options.IndexRoot, hash);
            if (Directory.Exists(store))
                gone &= TryDelete(() => Directory.Delete(store, recursive: true));
            return gone;
        }
    }

    private static bool TryDelete(Action delete)
    {
        try { delete(); return true; }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }

    private bool IsCurrent(string fullPath)
        => string.Equals(_currentVaultPath, fullPath, StringComparison.OrdinalIgnoreCase);

    // Caller holds _gate. Ends the active vault's lifetime and releases it.
    private void TearDown()
    {
        _lifetime.Cancel();
        _lifetime.Dispose();
        _lifetime = new CancellationTokenSource();
        _work = new Work();
        _scope?.Dispose();
        _provider?.Dispose();
        _scope = null;
        _provider = null;
        _vault = null;
        _currentVaultPath = null;
    }

    private static string Snippet(string? content)
    {
        if (string.IsNullOrEmpty(content)) return string.Empty;
        return content.Length <= 200 ? content : content[..200];
    }

    /// <summary>
    /// Disposes the active vault's scope and provider (SQLite connections, file handles).
    /// The shared embedder is owned by the caller — never disposed here.
    /// </summary>
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            _disposed = true;
            TearDown();
            _lifetime.Dispose();
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
