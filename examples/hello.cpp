#include <iostream>
#include <vector>

int main() {
    std::vector<int> xs{1, 2, 3, 5, 8};
    int sum = 0;
    for (int x : xs) sum += x;
    std::cout << "aegis/cpp: " << sum << "\n";
}
