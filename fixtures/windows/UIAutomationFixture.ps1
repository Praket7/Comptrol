param(
    [string]$StatePath = $env:COMPTROL_FIXTURE_STATE
)

Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore

$script:fixtureState = [ordered]@{
    status = 'Ready'
    submission_count = 0
    field_value = ''
    duplicate_one_count = 0
    duplicate_two_count = 0
    disabled_count = 0
}

function Write-FixtureState {
    if ($StatePath) {
        $stateDirectory = Split-Path -Parent $StatePath
        if ($stateDirectory) { New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null }
        [System.IO.File]::WriteAllText($StatePath, ($script:fixtureState | ConvertTo-Json -Compress), [System.Text.UTF8Encoding]::new($false))
    }
}
Write-FixtureState

$window = New-Object Windows.Window
$window.Title = 'Comptrol UIA Fixture'
$panel = New-Object Windows.Controls.StackPanel
$textBox = New-Object Windows.Controls.TextBox
$textBox.Text = ''
[Windows.Automation.AutomationProperties]::SetName($textBox, 'Synthetic value')
[Windows.Automation.AutomationProperties]::SetAutomationId($textBox, 'SyntheticValue')
$textBox.Add_TextChanged({
    $script:fixtureState.field_value = $textBox.Text
    Write-FixtureState
})
[void]$panel.Children.Add($textBox)
$button = New-Object Windows.Controls.Button
$button.Content = 'Submit'
[Windows.Automation.AutomationProperties]::SetName($button, 'Submit')
[Windows.Automation.AutomationProperties]::SetAutomationId($button, 'SubmitButton')
$button.Add_Click({
    $button.Content = 'Submitted'
    [Windows.Automation.AutomationProperties]::SetName($button, 'Submitted')
    $script:fixtureState.status = 'Submitted'
    $script:fixtureState.submission_count++
    Write-FixtureState
})
[void]$panel.Children.Add($button)

$duplicateOne = New-Object Windows.Controls.Button
$duplicateOne.Content = 'First duplicate'
[Windows.Automation.AutomationProperties]::SetName($duplicateOne, 'Duplicate')
[Windows.Automation.AutomationProperties]::SetAutomationId($duplicateOne, 'DuplicateOne')
$duplicateOne.Add_Click({
    $script:fixtureState.duplicate_one_count++
    Write-FixtureState
})
[void]$panel.Children.Add($duplicateOne)

$duplicateTwo = New-Object Windows.Controls.Button
$duplicateTwo.Content = 'Second duplicate'
[Windows.Automation.AutomationProperties]::SetName($duplicateTwo, 'Duplicate')
[Windows.Automation.AutomationProperties]::SetAutomationId($duplicateTwo, 'DuplicateTwo')
$duplicateTwo.Add_Click({
    $script:fixtureState.duplicate_two_count++
    Write-FixtureState
})
[void]$panel.Children.Add($duplicateTwo)

$disabled = New-Object Windows.Controls.Button
$disabled.Content = 'Disabled'
[Windows.Automation.AutomationProperties]::SetName($disabled, 'Disabled')
[Windows.Automation.AutomationProperties]::SetAutomationId($disabled, 'DisabledButton')
$disabled.IsEnabled = $false
$disabled.Add_Click({
    $script:fixtureState.disabled_count++
    Write-FixtureState
})
[void]$panel.Children.Add($disabled)
$window.Content = $panel
$window.Width = 320
$window.Height = 160
$window.Show()
$application = New-Object Windows.Application
[void]$application.Run()
