param(
    [Parameter(Mandatory)]
    [ValidateSet("Add", "Remove")]
    [string] $Action,

    [Parameter(Mandatory)]
    [string] $Directory,

    [ValidateSet("Process", "User")]
    [string] $Target = "User"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$current = [Environment]::GetEnvironmentVariable("Path", $Target)
$entries = [System.Collections.Generic.List[string]]::new()
if (-not [string]::IsNullOrEmpty($current)) {
    foreach ($pathEntry in ($current -split ";", 0, "SimpleMatch")) {
        $entries.Add($pathEntry)
    }
}

function Test-MatchesDirectory {
    param([string] $Candidate)

    $normalizedCandidate = [IO.Path]::GetFullPath($Candidate).TrimEnd("\")
    $normalizedDirectory = [IO.Path]::GetFullPath($Directory).TrimEnd("\")
    [string]::Equals(
        $normalizedCandidate,
        $normalizedDirectory,
        [StringComparison]::OrdinalIgnoreCase
    )
}

if ($Action -eq "Add") {
    $alreadyPresent = $false
    for ($index = 0; $index -lt $entries.Count; $index++) {
        if (Test-MatchesDirectory $entries[$index]) {
            $canonicalEntry = [IO.Path]::GetFullPath($entries[$index]).TrimEnd("\")
            if (-not [string]::Equals(
                $entries[$index],
                $canonicalEntry,
                [StringComparison]::Ordinal
            )) {
                $entries[$index] = $canonicalEntry
            }
            $alreadyPresent = $true
            break
        }
    }
    if (-not $alreadyPresent) {
        $entries.Add($Directory)
    }
} else {
    for ($index = $entries.Count - 1; $index -ge 0; $index--) {
        if (Test-MatchesDirectory $entries[$index]) {
            $entries.RemoveAt($index)
        }
    }
}

[Environment]::SetEnvironmentVariable("Path", ($entries -join ";"), $Target)
