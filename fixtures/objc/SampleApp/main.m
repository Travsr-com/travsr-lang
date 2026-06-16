// Entry-point fixture — exercises Dog, Animal, and NSString+Util.
// Verifies: C function definition (main) + cross-method call edges.
#import <Foundation/Foundation.h>
#import "Dog.h"

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        Dog *d = [[Dog alloc] initWithName:@"Rex" volume:0.8f];
        NSLog(@"%@", [d speak]);
        NSLog(@"%@", [d describe]);

        NSString *word = @"racecar";
        NSLog(@"isPalindrome: %d", [word travsr_isPalindrome]);
    }
    return 0;
}
