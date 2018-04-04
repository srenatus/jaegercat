#!/bin/bash

# reset sampling to default (when services ask -- every minute)
# default is probabilistic with rate 0.001

socat -v -d -T1 \
    TCP-LISTEN:5778,crlf,reuseaddr,fork \
    SYSTEM:"
        echo 'HTTP/1.1 200 OK'
        echo
        jo strategyType=PROBABILISTIC probabilisticSampling=\$(jo samplingRate=0.001)
    "
