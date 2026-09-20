# Captura la ventana de disky y la guarda como PNG en docs/screenshots/.
#
# Uso (con la app abierta):
#   powershell -File scripts/captura.ps1 -Nombre treemap
#   → docs/screenshots/treemap.png

param(
  [Parameter(Mandatory = $true)]
  [string]$Nombre,

  # Nombre del proceso de la app (sin .exe).
  [string]$Proceso = "disky"
)

$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class Win32 {
  [StructLayout(LayoutKind.Sequential)]
  public struct RECT { public int Left, Top, Right, Bottom; }

  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
  public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);

  // Ventanas top-level visibles de `pid`, ordenadas por área descendente.
  public static List<IntPtr> VisibleWindowsOf(uint pid) {
    var found = new List<IntPtr>();
    EnumWindows((h, l) => {
      uint wpid; GetWindowThreadProcessId(h, out wpid);
      if (wpid == pid && IsWindowVisible(h)) found.Add(h);
      return true;
    }, IntPtr.Zero);
    found.Sort((a, b) => {
      RECT ra, rb; GetWindowRect(a, out ra); GetWindowRect(b, out rb);
      long areaA = (long)(ra.Right - ra.Left) * (ra.Bottom - ra.Top);
      long areaB = (long)(rb.Right - rb.Left) * (rb.Bottom - rb.Top);
      return areaB.CompareTo(areaA);
    });
    return found;
  }
}
"@

# Evita capturas borrosas/escalonadas en pantallas con escalado.
[Win32]::SetProcessDPIAware() | Out-Null

$proc = Get-Process -Name $Proceso -ErrorAction SilentlyContinue
if (-not $proc) {
  throw "No hay proceso '$Proceso'. Arranca la app (npm run tauri dev)."
}

# La app puede exponer varias ventanas del mismo proceso (auxiliares ocultas,
# la principal visible): nos quedamos con la visible más grande con área real.
$handle = [IntPtr]::Zero
foreach ($p in $proc) {
  $wins = [Win32]::VisibleWindowsOf([uint32]$p.Id)
  foreach ($h in $wins) {
    $rect = New-Object Win32+RECT
    [Win32]::GetWindowRect($h, [ref]$rect) | Out-Null
    if (($rect.Right - $rect.Left) -gt 200 -and ($rect.Bottom - $rect.Top) -gt 200) {
      $handle = $h
      break
    }
  }
  if ($handle -ne [IntPtr]::Zero) { break }
}
if ($handle -eq [IntPtr]::Zero) {
  throw "El proceso '$Proceso' no tiene ninguna ventana visible. ¿Está minimizada en la bandeja?"
}

# Restaurar si está minimizada y traer al frente para que no tape otra ventana.
[Win32]::ShowWindow($handle, 9) | Out-Null   # 9 = SW_RESTORE
[Win32]::SetForegroundWindow($handle) | Out-Null
Start-Sleep -Milliseconds 500

$rect = New-Object Win32+RECT
[Win32]::GetWindowRect($handle, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bmp.Size)

$outDir = Join-Path $PSScriptRoot "..\docs\screenshots"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$out = Join-Path $outDir "$Nombre.png"
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)

$g.Dispose()
$bmp.Dispose()
Write-Host "Captura guardada en $out (${w}x${h})"
