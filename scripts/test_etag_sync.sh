#!/bin/bash

# Test script to validate ETag cache synchronization with actual file contents

set -e

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${YELLOW}=== Testing ETag Cache Synchronization ===${NC}"

# Build release binary if needed
if [ ! -f "./target/release/pwned-passwords-downloader-rs" ]; then
    echo "Building release binary..."
    cargo build --release
fi

TEST_DIR="test_etag_sync_$$"
mkdir -p "$TEST_DIR"

echo -e "\n${GREEN}Test: Download with interruption, then validate ETag/file consistency${NC}"

# Download for a short time then interrupt
echo "Starting download (will interrupt after 10 seconds)..."
(./target/release/pwned-passwords-downloader-rs \
    --combine \
    --output-directory "$TEST_DIR" \
    --max-concurrent-requests 32 \
    --quiet) &
PID=$!
sleep 10
kill -INT $PID 2>/dev/null || true
wait $PID 2>/dev/null || true

echo -e "\n${YELLOW}Analyzing results...${NC}"

# Get ranges from ETag cache
ETAG_RANGES=$(grep -o '"[0-9A-F]\{5\}"' "$TEST_DIR/.etag_cache.json" 2>/dev/null | tr -d '"' | sort || true)
ETAG_COUNT=$(echo "$ETAG_RANGES" | grep -v '^$' | wc -l | tr -d ' ')

# Get ranges from combined file
FILE_RANGES=$(grep "^# Range:" "$TEST_DIR/pwned-passwords-combined.txt" 2>/dev/null | awk '{print $3}' | sort || true)
FILE_COUNT=$(echo "$FILE_RANGES" | grep -v '^$' | wc -l | tr -d ' ')

echo "Ranges in ETag cache: $ETAG_COUNT"
echo "Ranges in file: $FILE_COUNT"

# Compare the sets
if [ "$ETAG_COUNT" -eq 0 ]; then
    echo -e "${YELLOW}No ETags found (download may have been too brief)${NC}"
    rm -rf "$TEST_DIR"
    exit 0
fi

# Check if every ETag range exists in the file
echo -e "\n${YELLOW}Validating: Every cached ETag must have corresponding data in file...${NC}"
MISSING_RANGES=""
for range in $ETAG_RANGES; do
    if ! echo "$FILE_RANGES" | grep -q "^${range}$"; then
        MISSING_RANGES="$MISSING_RANGES $range"
    fi
done

if [ -n "$MISSING_RANGES" ]; then
    echo -e "${RED}✗ FAILURE: Found ETags for ranges not in file!${NC}"
    echo -e "${RED}Missing ranges:$MISSING_RANGES${NC}"
    echo -e "\n${YELLOW}This means on resume, these ranges would be skipped (data loss!)${NC}"

    # Show first few for debugging
    echo -e "\n${YELLOW}First 10 ETags:${NC}"
    echo "$ETAG_RANGES" | head -10
    echo -e "\n${YELLOW}First 10 file ranges:${NC}"
    echo "$FILE_RANGES" | head -10

    rm -rf "$TEST_DIR"
    exit 1
else
    echo -e "${GREEN}✓ All cached ETags have corresponding data in file${NC}"
fi

# Check for extra ranges in file (shouldn't happen, but good to verify)
EXTRA_RANGES=""
for range in $FILE_RANGES; do
    if ! echo "$ETAG_RANGES" | grep -q "^${range}$"; then
        EXTRA_RANGES="$EXTRA_RANGES $range"
    fi
done

if [ -n "$EXTRA_RANGES" ]; then
    echo -e "${YELLOW}⚠ Warning: File contains ranges without ETags (count: $(echo $EXTRA_RANGES | wc -w | tr -d ' '))${NC}"
    echo -e "${YELLOW}This could happen if writes succeeded but ETag updates failed${NC}"
else
    echo -e "${GREEN}✓ No extra ranges in file${NC}"
fi

# Verify ranges in file are consecutive from 00000
echo -e "\n${YELLOW}Checking for gaps in sequence...${NC}"
EXPECTED=0
HAS_GAP=false
for range in $FILE_RANGES; do
    DECIMAL=$((16#$range))
    if [ $DECIMAL -ne $EXPECTED ]; then
        echo -e "${RED}✗ Gap detected! Expected $(printf "%05X" $EXPECTED), found $range${NC}"
        HAS_GAP=true
        break
    fi
    EXPECTED=$((EXPECTED + 1))
done

if [ "$HAS_GAP" = true ]; then
    echo -e "${RED}✗ FAILURE: File has gaps in sequence${NC}"
    rm -rf "$TEST_DIR"
    exit 1
else
    echo -e "${GREEN}✓ File has no gaps (ranges 00000 to $(printf "%05X" $((EXPECTED - 1))))${NC}"
fi

# Test resume doesn't skip the ranges that ARE in the file
echo -e "\n${GREEN}Testing resume doesn't re-download ranges that are already in file...${NC}"
RANGES_BEFORE=$FILE_COUNT

(./target/release/pwned-passwords-downloader-rs \
    --combine \
    --output-directory "$TEST_DIR" \
    --max-concurrent-requests 32 \
    --resume \
    --quiet) &
PID=$!
sleep 5
kill -INT $PID 2>/dev/null || true
wait $PID 2>/dev/null || true

FILE_RANGES_AFTER=$(grep "^# Range:" "$TEST_DIR/pwned-passwords-combined.txt" 2>/dev/null | awk '{print $3}' | sort || true)
FILE_COUNT_AFTER=$(echo "$FILE_RANGES_AFTER" | grep -v '^$' | wc -l | tr -d ' ')

echo "Ranges after resume: $FILE_COUNT_AFTER"
echo "Ranges added: $((FILE_COUNT_AFTER - RANGES_BEFORE))"

# Verify all original ranges still exist
ALL_PRESENT=true
for range in $FILE_RANGES; do
    if ! echo "$FILE_RANGES_AFTER" | grep -q "^${range}$"; then
        echo -e "${RED}✗ Range $range disappeared after resume!${NC}"
        ALL_PRESENT=false
    fi
done

if [ "$ALL_PRESENT" = true ]; then
    echo -e "${GREEN}✓ All original ranges still present after resume${NC}"
else
    echo -e "${RED}✗ FAILURE: Data loss on resume${NC}"
    rm -rf "$TEST_DIR"
    exit 1
fi

# Final verification: still no gaps
echo -e "\n${YELLOW}Final gap check...${NC}"
EXPECTED=0
HAS_GAP=false
for range in $FILE_RANGES_AFTER; do
    DECIMAL=$((16#$range))
    if [ $DECIMAL -ne $EXPECTED ]; then
        echo -e "${RED}✗ Gap after resume! Expected $(printf "%05X" $EXPECTED), found $range${NC}"
        HAS_GAP=true
        break
    fi
    EXPECTED=$((EXPECTED + 1))
done

if [ "$HAS_GAP" = true ]; then
    echo -e "${RED}✗ FAILURE: Gaps appeared after resume${NC}"
    rm -rf "$TEST_DIR"
    exit 1
else
    echo -e "${GREEN}✓ No gaps after resume${NC}"
fi

echo -e "\n${GREEN}=== All ETag synchronization tests PASSED ===${NC}"

# Cleanup
rm -rf "$TEST_DIR"