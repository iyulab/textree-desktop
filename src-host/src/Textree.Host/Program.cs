using System.Text.Json;
using IronProw.Core;
using IronProw.IronHive;
using LMSupply.Embedder;
using Textree.Host;
using Textree.Host.Contracts;
using Textree.Host.Rag;
using Textree.Host.Diagnostics;
using TauriKit.Sidecar.Loopback;
// Program.cs also constructs Textree.Host.Rag.ChatMessage below (the /chat DTO mapping); alias
// M.E.AI's IChatClient explicitly rather than `using Microsoft.Extensions.AI;`, which would make
// the unqualified `ChatMessage` reference ambiguous between the two types.
using IChatClient = Microsoft.Extensions.AI.IChatClient;

// ── One-shot commands ─────────────────────────────────────────────────────────
// `probe-byo` checks the BYO settings in the environment, prints one JSON line and exits — no
// server, no token, no models. Settings ▸ Advanced ▸ Test connection runs it.
if (args is [ByoProbeCommand.Name, ..])
{
    var probeEnv = Environment.GetEnvironmentVariables()
        .Cast<System.Collections.DictionaryEntry>()
        .ToDictionary(e => (string)e.Key, e => (string?)(e.Value?.ToString()));
    Environment.Exit(await ByoProbeCommand.RunAsync(probeEnv, Console.Out));
}

// ── DI / builder ──────────────────────────────────────────────────────────────
// The embedder loads in the background so the host begins accepting requests
// immediately. Rust's Plan B lifecycle (poll /health with retry on connection-
// refused) still works on cold download; now /health answers promptly with
// embedderReady=false rather than remaining unreachable until the model is on disk.
var opts = new TextreeHostOptions();
var status = new ModelStatus();
// Empty until the background load finishes, keeping EmbedderReady==false and /search
// returning "warming" meanwhile.
var embedder = new EmbedderSlot();

// The local model (phi-4-mini) is an ONNX Runtime GenAI model. LMSupply.Generator ships the
// GGUF path only; the ONNX backend is its own package and has to be registered before any
// model loads, or loading fails with an error naming the missing registration.
LMSupply.Generator.Onnx.OnnxGeneratorBackend.Register();

var builder = WebApplication.CreateBuilder(args);
// 127.0.0.1 only, on a port the system picks; the readiness line at the end says which.
builder.WebHost.ConfigureKestrel(k => k.Listen(System.Net.IPAddress.Loopback, 0));
// Every route requires the app's token (registered below, once the settings are read); a route
// added later is closed until it is marked open.
var credentials = new LoopbackCredentials();
builder.Services.AddLoopbackAuthentication(credentials);
builder.Services.AddSingleton(opts);
builder.Services.AddSingleton(status);
builder.Services.AddSingleton(embedder);
// This dictionary is also reused below to decide the ITextGenerator backend (TEXTREE_BYO_PRESET),
// so Environment.GetEnvironmentVariables() is only ever materialized once.
var envDict = Environment.GetEnvironmentVariables()
    .Cast<System.Collections.DictionaryEntry>()
    .ToDictionary(e => (string)e.Key, e => (string?)(e.Value?.ToString()));
// Failures go to the reports folder the shell names, as one more segment of the shell's own stream;
// the shell verifies and sends them. Without the variable nothing is written.
envDict.TryGetValue(FailureReports.ReportsDirVar, out var reportsDir);
builder.Services.AddSingleton(FailureReports.Open(reportsDir));
builder.Services.AddSingleton<IEmbedderLoader, DefaultEmbedderLoader>();
builder.Services.AddSingleton<VaultManager>();
// Local CPU text generator for /chat, OR a user-configured BYO OpenAI-compatible server
// when TEXTREE_BYO_PRESET is set. Singleton either way: the local path
// loads its model once and reuses it; the remote path is stateless per-request but a singleton
// keeps construction cheap and consistent with the local path's lifetime. Tests replace this
// with a stub via ConfigureTestServices.
var byoPreset = TextGeneratorSelection.SelectedPreset(envDict);
if (byoPreset is not null)
{
    var byoBaseUrl = envDict.GetValueOrDefault("TEXTREE_BYO_BASE_URL") ?? "";
    var byoApiKey = envDict.GetValueOrDefault("TEXTREE_BYO_API_KEY");
    // Settings ▸Advanced now requires a non-empty model before Save is enabled, so this env var
    // should always be set in practice. "default" here is a plain defensive fallback for any
    // caller that bypasses the UI (stale localStorage from before that gate existed, manual
    // testing, etc.) — it is NOT a real model name and does NOT resolve to the server's actual
    // default; a request using it will still 404/fail against a real OpenAI-compatible server.
    var byoModel = envDict.GetValueOrDefault("TEXTREE_BYO_MODEL") ?? "default";

    // iron-prow gateway with exactly one provider (selection inert). The bridge owns provider
    // construction (substrate); the host only adapts the resulting IChatClient into ITextGenerator.
    // Settings the catalogue refuses never reach a provider: /health reports why instead.
    var byoEndpoint = ByoProviderRegistration.EndpointFor(byoPreset, byoBaseUrl, byoApiKey);
    if (ByoPresets.Validate(byoEndpoint) is { } unusable)
    {
        builder.Services.AddSingleton<ITextGenerator>(new UnusableTextGenerator(unusable));
    }
    else
    {
        ByoProviderRegistration.Register(builder.Services.AddIronProw(), byoEndpoint, byoModel);
        builder.Services.AddSingleton<ITextGenerator>(sp =>
            new RemoteChatTextGenerator(sp.GetRequiredService<IChatClient>(), byoModel));
    }
}
else
{
    builder.Services.AddSingleton<ITextGenerator, LocalTextGenerator>();
}

var app = builder.Build();

// Only the desktop app that started this process may call it. Listening on 127.0.0.1 keeps other
// machines out, not other programs on this one — any of them, under any account, could otherwise
// index or search a folder of its choosing as this user, or spend the user's AI key. The app hands
// a fresh token to each run; every request must carry it. Read after Build so a test host's
// settings count. Without a token the host refuses to start rather than serve everyone.
var hostToken = app.Configuration[HostToken.Setting];
if (string.IsNullOrEmpty(hostToken) || hostToken.Length < HostToken.MinimumLength)
{
    throw new InvalidOperationException(
        $"{HostToken.Setting} is missing or shorter than {HostToken.MinimumLength} characters; the host serves only the app that started it.");
}
credentials.Register(hostToken, HostToken.Caller);
// A web page whose own name was pointed at 127.0.0.1 reaches the port but names itself, not a
// loopback address: refused before anything else runs.
app.UseLoopbackHostCheck();
app.UseAuthentication();
app.UseAuthorization();

// Background embedder load — host starts listening immediately.
// The background task resolves IEmbedderLoader from the DI container so that
// tests can inject a stub via ConfigureTestServices and have it take effect here.
status.SetEmbedderPhase(ModelPhase.Downloading);
_ = Task.Run(async () =>
{
    try
    {
        var model = await app.Services.GetRequiredService<IEmbedderLoader>()
            .LoadAsync(opts.EmbeddingModel, status.EmbedderProgress, CancellationToken.None);
        status.SetEmbedderPhase(ModelPhase.Loading);
        embedder.Set(model);
        status.SetEmbedderPhase(ModelPhase.Ready);
    }
    catch (Exception ex)
    {
        var phaseAtFailure = status.Embedder.Phase; // Downloading/Loading/... before we flip to Error
        status.SetEmbedderError(ex.Message);        // local-only; message keeps the path (data sovereignty OK locally)
        app.Services.GetRequiredService<IFailureReports>()
            .ReportError(HostEvent.ForModelFailure(phaseAtFailure), "embedder", phaseAtFailure, ex);
    }
});

var mgr = app.Services.GetRequiredService<VaultManager>();
var indexLog = app.Services.GetRequiredService<ILoggerFactory>().CreateLogger("Textree.Host.Index");
var reindexLog = app.Services.GetRequiredService<ILoggerFactory>().CreateLogger("Textree.Host.Reindex");
var genLog = app.Services.GetRequiredService<ILoggerFactory>().CreateLogger("Textree.Host.Generation");

// ── Endpoints ─────────────────────────────────────────────────────────────────

app.MapGet("/health", (ITextGenerator gen, ModelStatus modelStatus) =>
{
    static object? Dl(ModelSnapshot s) =>
        s.Phase is ModelPhase.Downloading or ModelPhase.Loading
            ? new
            {
                phase = s.Phase == ModelPhase.Loading ? "loading" : "downloading",
                overallPercent = s.OverallPercent,
                bytesDownloaded = s.BytesDownloaded,
                totalBytes = s.TotalBytes,
                fileIndex = s.FileIndex,
                fileCount = s.FileCount,
            }
            : null;
    return Results.Ok(new
    {
        status = "ok",
        embedderReady = mgr.EmbedderReady,
        generatorReady = gen.Ready,
        generatorError = gen.LastError,
        embedderError = modelStatus.Embedder.Error,
        embedderDownload = Dl(modelStatus.Embedder),
        generatorDownload = Dl(modelStatus.Generator),
        embedderProviders = embedder.ActiveProviders,
        generatorProviders = gen.ActiveProviders,
    });
});

app.MapPost("/prepare-generation", (ITextGenerator gen) =>
{
    // Idempotent: kick off model load in the background, return immediately.
    // PrepareAsync is internally idempotent (double-checked SemaphoreSlim), so
    // concurrent or repeated calls are safe.
    _ = Task.Run(async () =>
    {
        try { await gen.PrepareAsync(CancellationToken.None); }
        catch (Exception ex)
        {
            genLog.LogError(ex, "Generation model PrepareAsync failed");
            app.Services.GetRequiredService<IFailureReports>()
                .ReportError(HostEvent.GenerationPrepareFailed, "generator", ModelPhase.Error, ex);
        }
    });
    return Results.Accepted();
});

app.MapPost("/index", async (IndexRequest req, CancellationToken ct) =>
{
    // Boundary validation: empty paths would reach Path.GetFullPath("") and silently
    // resolve to CWD — a scope-correctness hazard. Reject at the edge.
    if (string.IsNullOrWhiteSpace(req.VaultPath) || string.IsNullOrWhiteSpace(req.Path))
        return Results.BadRequest("VaultPath and Path required");

    // A source that is already gone is gone whether or not the model has loaded yet.
    if (!File.Exists(req.Path))
        return Results.NotFound(new { status = "gone" });

    // Nothing to index with until the model has loaded; the caller may try again.
    if (!mgr.EmbedderReady)
        return Results.Json(new { status = "warming" }, statusCode: StatusCodes.Status503ServiceUnavailable);

    try
    {
        await mgr.MemorizeAsync(req.VaultPath, req.Path, ct);
    }
    catch (Exception ex)
    {
        switch (IndexFailurePolicy.Classify(ex, ct.IsCancellationRequested))
        {
            case IndexFailureKind.SourceGone:
                indexLog.LogDebug(ex, "Index skipped: the source is gone");
                return Results.NotFound(new { status = "gone" });

            case IndexFailureKind.CallerGone:
                // Let the server handle the abandoned request the way it normally does.
                indexLog.LogDebug("Index cancelled: the caller went away");
                throw;

            default:
                // A real failure must not vanish into an unhandled 500: the caller indexes in the
                // background and never reads the result, so this is the only place it is visible.
                indexLog.LogError(ex, "Index failed");
                app.Services.GetRequiredService<IFailureReports>()
                    .ReportError(HostEvent.IndexFailed, "embedder", ModelPhase.Error, ex);
                throw;
        }
    }
    return Results.Ok(new { status = "ok" });
});

app.MapPost("/reindex", (ReindexRequest req) =>
{
    if (string.IsNullOrWhiteSpace(req.VaultPath))
        return Results.BadRequest("VaultPath required");

    // Same answer as /index before the model has loaded: not yet, try again.
    if (!mgr.EmbedderReady)
        return Results.Json(new { status = "warming" }, statusCode: StatusCodes.Status503ServiceUnavailable);

    _ = Task.Run(async () =>
    {
        try { await mgr.ReindexAsync(req.VaultPath, CancellationToken.None); }
        catch (Exception ex)
        {
            reindexLog.LogError(ex, "Reindex failed for {Vault}", req.VaultPath);
            app.Services.GetRequiredService<IFailureReports>()
                .ReportError(HostEvent.ReindexFailed, "embedder", ModelPhase.Error, ex);
        }
    });
    return Results.Accepted();
});

app.MapPost("/search", async (SearchRequest req, CancellationToken ct) =>
{
    // Empty Query is allowed (empty results are harmless); empty Vault/Scope mis-scopes silently.
    if (string.IsNullOrWhiteSpace(req.VaultPath) || string.IsNullOrWhiteSpace(req.ScopePath))
        return Results.BadRequest("VaultPath and ScopePath required");

    if (!mgr.EmbedderReady)
        return Results.Ok(new SearchResponse([], "warming"));

    var hits = await mgr.SearchAsync(req.VaultPath, req.Query, req.ScopePath, req.Limit, ct);
    var results = hits
        .Select(h => new SearchHitDto(h.SourcePath, h.Snippet, h.Score))
        .ToList();
    return Results.Ok(new SearchResponse(results, "ok"));
});

app.MapPost("/chat", async (ChatRequestDto req, ITextGenerator gen, HttpContext ctx, CancellationToken ct) =>
{
    if (req.Messages is null || req.Messages.Count == 0)
        return Results.BadRequest("messages required");

    // Lazy-load safety net: if the model has not been prepared yet (no prior
    // /prepare-generation call), load it now before we begin streaming.
    if (!gen.Ready)
    {
        try
        {
            await gen.PrepareAsync(ct);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            app.Services.GetRequiredService<IFailureReports>()
                .ReportError(HostEvent.GenerationPrepareFailed, "generator", ModelPhase.Error, ex);
            throw;
        }
    }

    ctx.Response.ContentType = "text/event-stream";
    var msgs = req.Messages.Select(m => new ChatMessage(m.Role, m.Content)).ToList();
    // The desktop app never sets MaxTokens, so this default IS the effective cap: 512 protects
    // the bundled CPU model; a BYO backend gets 4096 (see TextGeneratorSelection.DefaultMaxTokens).
    var genOpts = new GenerationOptions(MaxTokens: req.MaxTokens ?? TextGeneratorSelection.DefaultMaxTokens(byoPreset));

    // ct = HttpContext.RequestAborted -> client disconnect stops generation, frees CPU.
    string? finishReason = null;
    await foreach (var chunk in gen.GenerateAsync(msgs, genOpts, ct))
    {
        if (chunk.FinishReason is not null) finishReason = chunk.FinishReason;
        if (string.IsNullOrEmpty(chunk.Text)) continue;
        var json = JsonSerializer.Serialize(new { choices = new[] { new { delta = new { content = chunk.Text } } } });
        await ctx.Response.WriteAsync($"data: {json}\n\n", ct);
        await ctx.Response.Body.FlushAsync(ct);
    }
    // Why the answer ended, the OpenAI way: an empty delta carrying finish_reason, just before
    // [DONE]. Only when the backend said — readers that do not look for it see the stream as before.
    if (finishReason is not null)
    {
        var json = JsonSerializer.Serialize(new { choices = new[] { new { delta = new { }, finish_reason = finishReason } } });
        await ctx.Response.WriteAsync($"data: {json}\n\n", ct);
    }
    await ctx.Response.WriteAsync("data: [DONE]\n\n", ct);
    // The handler streams directly into ctx.Response (SSE); there is no object to return.
    // Results.Empty is a formality required by the IResult-returning delegate signature — it
    // writes nothing extra to the response. On client disconnect the OperationCanceledException
    // propagates before this line is reached, so it is unreachable on the abort path.
    return Results.Empty;
});

app.MapPost("/shutdown", (IHostApplicationLifetime life) =>
{
    life.StopApplication();
    return Results.Accepted();
});

await app.RunAnnouncingAsync(HostToken.ReadyPrefix);

// Exposes the implicit Program class for WebApplicationFactory<Program> in tests
// (top-level statements compile to an internal Program; the partial makes it addressable).
public partial class Program { }
