# Run idf.py for this cell without `export.ps1`.
#
# The IDF on this box (v5.5.1 under ~/.espressif) has every tool a BUILD
# needs, but `export.ps1` refuses to activate while optional tools it does not
# use here (openocd, ccache, dfu-util, idf-exe) are missing. So this puts the
# installed toolchain, CMake and Ninja on PATH and runs idf.py through the
# IDF's own Python environment.
#
#   ./idf.ps1 build
#   ./idf.ps1 -p COM4 flash
# No param() block: idf.py's own -D/-B/-p flags must pass through untouched,
# and a declared parameter set makes PowerShell try to bind them.
$Pass = $args
$ErrorActionPreference = "Stop"
$esp = "$env:USERPROFILE\.espressif"
$env:IDF_PATH = "$esp\esp-idf\v5.5.1"
$env:IDF_TOOLS_PATH = $esp
$env:IDF_PYTHON_ENV_PATH = "$esp\python_env\idf5.5_py3.11_env"
$env:ESP_ROM_ELF_DIR = "$esp\tools\esp-rom-elfs\20241011\"
$tools = @(
    "$esp\tools\xtensa-esp-elf\esp-14.2.0_20241119\xtensa-esp-elf\bin",
    "$esp\tools\cmake\3.30.2\bin",
    "$esp\tools\ninja\1.12.1",
    "$env:IDF_PYTHON_ENV_PATH\Scripts",
    "$env:IDF_PATH\tools"
)
$env:PATH = ($tools -join ";") + ";" + $env:PATH
& "$env:IDF_PYTHON_ENV_PATH\Scripts\python.exe" "$env:IDF_PATH\tools\idf.py" @Args
exit $LASTEXITCODE
