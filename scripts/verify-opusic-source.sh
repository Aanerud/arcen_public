#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
# Copyright (C) 2026 TUTTIFRUTTI AS

set -euo pipefail
exec bash "$(dirname "${BASH_SOURCE[0]}")/verify-vendored-source.sh" opusic-sys-0.7.3-arcen1
