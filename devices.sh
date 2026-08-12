#!/bin/bash

echo "Searching for devices..."
echo "----------------------------------------"

# Find all devices
devices=()
i=0

while IFS= read -r line; do
    if [[ $line == *"N: Name="* ]]; then
        name=$(echo "$line" | sed 's/.*"\(.*\)".*/\1/')
    fi
    
    if [[ $line == *"H: Handlers="* ]] && [[ $line == *"kbd"* ]]; then
        event=$(echo "$line" | grep -o 'event[0-9]*')
        if [[ -n $event ]]; then
            devices+=("$event")
            echo "[$i] $name -> /dev/input/$event"
            ((i++))
        fi
    fi
done < /proc/bus/input/devices

if [[ ${#devices[@]} -eq 0 ]]; then
    echo "No keyboard found!"
    exit 1
fi

echo "----------------------------------------"

# If multiple devics found, let user choose
if [[ ${#devices[@]} -eq 1 ]]; then
    echo "Using: /dev/input/${devices[0]}"
    echo "export BAAN_KEYBOARD_PATH=/dev/input/${devices[0]}"
else
    echo "Multiple keyboards found. Choose one:"
    read -p "Enter number (0-$((${#devices[@]}-1))): " choice
    echo "Selected: /dev/input/${devices[$choice]}"
    echo "export BAAN_KEYBOARD_PATH=/dev/input/${devices[$choice]}"
fi