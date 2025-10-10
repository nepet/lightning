#!/usr/bin/env bash
# Subcommands:
#   openingfee <payment_size_msat> <proportional> <min_fee_msat>

set -euo pipefail

cmd_openingfee() {
    # Calculate according to BLIP52:
    # opening_fee = ((payment_size_msat * proportional) + 999999) / 10000000
    # If opening_fee < min_fee_msat, set it to min_fee_msat.

    if [[ $# -lt 3 ]]; then
        echo "Usage: $0 openingfee <payment_size_msat> <proportional> <min_fee_msat>" >&2
        exit 1
    fi

    local payment_size_msat=$1
    local proportional=$2
    local min_fee_msat=$3

    local opening_fee=$(( (payment_size_msat * proportional + 999999) / 10000000 ))

    if (( opening_fee < min_fee_msat )); then
        opening_fee=$min_fee_msat
    fi

    echo "$opening_fee"
}

show_help() {
    cat <<EOF
Usage: $0 <command> [args...]

Commands:
  openingfee   Calculate the opening fee
  help         Show this help

Examples:
  $0 openingfee 2000000 75 1000
EOF
}

main() {
    local cmd=${1:-help}
    shift || true

    case "$cmd" in
        openingfee) cmd_openingfee "$@" ;;
        help|--help|-h) show_help ;;
        *)
            echo "Unknown command: $cmd" >&2
            show_help
            exit 1
            ;;
    esac
}

main "$@"
