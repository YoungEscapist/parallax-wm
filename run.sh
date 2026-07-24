#!/bin/bash
~/dev/dawn/target/release/dawn --tty &
P=$!; sleep ${1:-10}; kill -9 $P 2>/dev/null; sudo chvt 1
cat /tmp/dawn.log 2>/dev/null
