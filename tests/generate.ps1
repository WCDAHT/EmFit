$dir = $PWD
New-Item -ItemType Directory -Path $dir -Force | Out-Null

1..3 | ForEach-Object {
    $fs = [System.IO.File]::Create((Join-Path $dir "file$_.bin"))
    $fs.SetLength(100MB)
    $fs.Close()
}