#Requires -Version 5.1

<#
.SYNOPSIS
    Runs ALMA-NV in a Docker container on Windows
.DESCRIPTION
    This script builds and runs the ALMA-NV Docker container with the necessary privileges
    for disk operations. It's the Windows PowerShell equivalent of run-alma.sh.
.PARAMETER Arguments
    Arguments to pass to the alma command inside the container
.EXAMPLE
    .\run-alma.ps1 --help
    Shows ALMA help
.EXAMPLE
    .\run-alma.ps1 create --preset installer
    Runs ALMA with the installer preset
#>

[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments)]
    [string[]]$Arguments = @()
)

# Set strict mode for better error handling
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Test-DockerAvailable {
    try {
        $null = docker info 2>$null
        return $true
    }
    catch {
        return $false
    }
}

function Test-DockerImageExists {
    param([string]$ImageName)

    try {
        $null = docker image inspect $ImageName 2>$null
        return $true
    }
    catch {
        return $false
    }
}

function Write-Warning-Message {
    Write-Host "WARNING: ALMA will run with privileged access to devices." -ForegroundColor Yellow
    Write-Host "This is required for disk operations but has security implications." -ForegroundColor Yellow
    Write-Host ""
}

function Write-Error-Message {
    param([string]$Message)
    Write-Host "ERROR: $Message" -ForegroundColor Red
}

# Main execution
try {
    # Check if Docker is available
    if (-not (Test-DockerAvailable)) {
        Write-Error-Message "Cannot access Docker. Make sure:"
        Write-Host "  1. Docker Desktop is running" -ForegroundColor Red
        Write-Host "  2. You have permission to access Docker" -ForegroundColor Red
        Write-Host "  3. You are in the 'docker-users' group" -ForegroundColor Red
        exit 1
    }

    Write-Warning-Message

    # Check if the Docker image exists, build it if not
    if (-not (Test-DockerImageExists "alma-nv")) {
        Write-Host "Building ALMA Docker image..." -ForegroundColor Green
        docker build -t alma-nv .
        if ($LASTEXITCODE -ne 0) {
            Write-Error-Message "Failed to build Docker image"
            exit $LASTEXITCODE
        }
    }

    # Get the current directory in a format Docker can understand
    $CurrentDir = $PWD.Path

    # Convert Windows path to Docker volume format if needed
    if ($CurrentDir -match '^[A-Za-z]:') {
        # Convert C:\path to /c/path format for Docker
        $CurrentDir = $CurrentDir -replace '^([A-Za-z]):', '/$1'
        $CurrentDir = $CurrentDir -replace '\\', '/'
        $CurrentDir = $CurrentDir.ToLower()
    }

    # Build Docker arguments
    $DockerArgs = @(
        "run"
        "--rm"
        "-it"
        "--privileged"
        "-v", "/var/run/docker.sock:/var/run/docker.sock"
        "-v", "${CurrentDir}:/work"
        "alma-nv"
    )

    # Add user arguments
    $DockerArgs += $Arguments

    Write-Host "Running: docker $($DockerArgs -join ' ')" -ForegroundColor Cyan

    # Execute Docker command
    & docker $DockerArgs
    exit $LASTEXITCODE
}
catch {
    Write-Error-Message "An unexpected error occurred: $($_.Exception.Message)"
    exit 1
}
