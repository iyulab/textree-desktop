using Textree.Host.Rag;
using Xunit;

public class VaultHashTests
{
    [Fact]
    public void SamePathNormalizedToSameHash()
    {
        // Spelled the way this machine spells paths: a Windows-shaped literal is a relative file
        // name elsewhere, and `\` is an ordinary character in it.
        var vault = Path.Combine(Path.GetTempPath(), "data", "vault");
        var a = VaultHash.For(vault);
        var b = VaultHash.For(vault + Path.DirectorySeparatorChar);    // trailing separator
        var c = VaultHash.For(Path.Combine(Path.GetTempPath(), "data", "VAULT")
            .Replace(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar)); // separators + casing
        Assert.Equal(a, b);
        Assert.Equal(a, c);
    }

    [Fact]
    public void DifferentPathsDifferHash()
        => Assert.NotEqual(VaultHash.For(@"C:\data\v1"), VaultHash.For(@"C:\data\v2"));

    [Fact]
    public void HashIs16HexChars()
        => Assert.Matches("^[0-9a-f]{16}$", VaultHash.For(@"C:\x"));
}
