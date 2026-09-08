#!/bin/sh
# Used only by the process-cleanup test. Ignores the Qwen CLI arguments.
printf 'worker started\n'
(sleep 2; touch child-survived) &
sleep 30
