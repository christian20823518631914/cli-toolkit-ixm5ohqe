package main

import "fmt"

func main() {
    xs := []int{1, 2, 3, 5, 8}
    sum := 0
    for _, x := range xs {
        sum += x
    }
    fmt.Println("aegis/go:", sum)
}
