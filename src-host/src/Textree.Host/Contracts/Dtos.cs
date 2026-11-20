namespace Textree.Host.Contracts;

public record IndexRequest(string VaultPath, string Path);
public record ReindexRequest(string VaultPath);
public record ForgetRequest(string VaultPath);
public record SearchRequest(string VaultPath, string Query, string ScopePath, int Limit = 10);
public record SearchHitDto(string Path, string Snippet, float Score);
public record SearchResponse(IReadOnlyList<SearchHitDto> Results, string Status);
