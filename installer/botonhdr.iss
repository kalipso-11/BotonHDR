; =====================================================================
;  BotonHDR 1.1 — Script de instalador (Inno Setup 6)
;
;  Para compilar (desde la raíz del proyecto, con Inno Setup 6 instalado):
;    "<ruta de Inno Setup 6>\ISCC.exe" installer\botonhdr.iss
;
;  Salida: installer\output\BotonHDR-Setup-1.1.exe
;
;  Comportamiento:
;  - Instala en C:\Program Files\BotonHDR (pide elevar permisos, UAC).
;  - Tarea opcional: acceso directo en el escritorio (marcado por defecto).
;  - Menú Inicio: carpeta "BotonHDR" + entrada (por defecto).
;  - Página final: checkbox "Ejecutar BotonHDR" (marcado por defecto).
;  - Desinstalador registrado en "Agregar o quitar programas".
;  - Si la app esta corriendo, el desinstalador la cierra antes de borrar
;    nada ([Code] + TaskKill en usAppMutexCheck) — no deja exe/carpeta
;    huérfanos.
;  - NO borra config ni logs: viven en %APPDATA%\BotonHDR2 (se conservan).
; =====================================================================

#define MyAppName "BotonHDR"
#define MyAppVersion "1.1"
#define MyAppPublisher "kalipso-11"
#define MyAppExeName "BotonHDR.exe"

[Setup]
; Id estable: lo mismo en todas las versiones (actualiza in-place).
AppId={{930C1F4D-7961-41A9-A236-6D7B6C134A2C}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
CloseApplications=yes
OutputDir=output
OutputBaseFilename={#MyAppName}-Setup-{#MyAppVersion}
SetupIconFile=..\assets\botonhdr.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
VersionInfoVersion={#MyAppVersion}.0
ArchitecturesInstallIn64BitMode=x64compatible
ShowLanguageDialog=yes
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"

[Tasks]
; "desktopicon" usa la descripción localizada automática del lenguaje elegido.
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; skipifsilent: en instalación silenciosa no auto-inicia (se lanza aparte).
Filename: "{app}\{#MyAppExeName}"; Description: "Ejecutar {#MyAppName}"; Flags: nowait postinstall skipifsilent

; Nota: sin [UninstallDelete] a propósito — la config y los logs viven en
; %APPDATA%\BotonHDR2 y deben conservarse al desinstalar.

[Code]
// Si BotonHDR esta en ejecucion, cerramos el proceso ANTES de que el
// desinstalador borre archivos (usAppMutexCheck es el primer paso).
// Con esto, desinstalar con la app abierta ya no deja .exe/carpeta
// huérfanos.

function Sleep(Milliseconds: LongWord): LongWord;
external 'Sleep@kernel32.dll stdcall';

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usAppMutexCheck then
  begin
    // Cierra BotonHDR si esta corriendo (Inno no trae TaskKill; usamos el
    // taskkill.exe del sistema). Exit code 0 = cerro proceso(s), 128 = no
    // estaba corriendo; en ambos casos seguimos con la desinstalacion.
    if Exec('taskkill.exe', '/F /IM BotonHDR.exe', '', SW_HIDE,
        ewWaitUntilTerminated, ResultCode) and (ResultCode = 0) then
      Sleep(2000);  // espera a que el SO libere el .exe
  end;
end;
