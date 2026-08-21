// Category fixture: verifies that category methods map to the base class symbol.
// Methods here must emit `objc . . 0.0.0 NSString#...` NOT `NSString(Util)#...`.
#import <Foundation/Foundation.h>

@interface NSString (Util)
- (BOOL)travsr_isPalindrome;
- (NSString *)travsr_reversed;
@end

@implementation NSString (Util)

- (BOOL)travsr_isPalindrome {
    NSString *rev = [self travsr_reversed];
    return [self isEqualToString:rev];
}

- (NSString *)travsr_reversed {
    NSMutableString *reversed = [NSMutableString stringWithCapacity:self.length];
    for (NSInteger i = (NSInteger)self.length - 1; i >= 0; i--) {
        [reversed appendFormat:@"%C", [self characterAtIndex:(NSUInteger)i]];
    }
    return reversed;
}

@end
