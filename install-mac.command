#!/bin/bash
# HoosierSDR macOS installer — double-click this file to install.
# It downloads and runs the latest installer, so it never goes stale.
# If macOS blocks it ("unidentified developer"), right-click → Open instead.

curl -fsSL https://raw.githubusercontent.com/duderayuh/HoosierSDR/main/tools/install-mac.sh | bash

echo
printf '\033[1;32mInstall finished.\033[0m Press return to close this window.\n'
read -r _
