param(
    [string]$StatePath
)

Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore

$state = [ordered]@{ status = 'Ready'; decoy_count = 0 }
if ($StatePath) {
    $directory = Split-Path -Parent $StatePath
    if ($directory) { New-Item -ItemType Directory -Force -Path $directory | Out-Null }
    [System.IO.File]::WriteAllText($StatePath, ($state | ConvertTo-Json -Compress), [System.Text.UTF8Encoding]::new($false))
}

$window = New-Object Windows.Window
$window.Title = 'Comptrol UIA Decoy Fixture'
$button = New-Object Windows.Controls.Button
$button.Content = 'Unrelated'
[Windows.Automation.AutomationProperties]::SetName($button, 'Unrelated')
[Windows.Automation.AutomationProperties]::SetAutomationId($button, 'UnrelatedButton')
$button.Add_Click({
    $state.decoy_count++
    if ($StatePath) {
        [System.IO.File]::WriteAllText($StatePath, ($state | ConvertTo-Json -Compress), [System.Text.UTF8Encoding]::new($false))
    }
})
$window.Content = $button
[void]$window.ShowDialog()
