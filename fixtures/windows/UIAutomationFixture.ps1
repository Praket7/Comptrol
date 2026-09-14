Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore

$window = New-Object Windows.Window
$window.Title = 'Comptrol UIA Fixture'
$panel = New-Object Windows.Controls.StackPanel
$button = New-Object Windows.Controls.Button
$button.Content = 'Submit'
[Windows.Automation.AutomationProperties]::SetName($button, 'Submit')
$button.Add_Click({
    $button.Content = 'Submitted'
    [Windows.Automation.AutomationProperties]::SetName($button, 'Submitted')
})
[void]$panel.Children.Add($button)
$window.Content = $panel
$window.Width = 320
$window.Height = 160
$window.Show()
$application = New-Object Windows.Application
[void]$application.Run()
