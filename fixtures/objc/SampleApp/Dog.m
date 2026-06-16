// Subclass implementation fixture.
// Verifies: method call edges ([super init], [NSString stringWithFormat:]).
#import "Dog.h"

@implementation Dog

- (instancetype)initWithName:(NSString *)name volume:(float)volume {
    if (self = [super initWithName:name]) {
        _volume = volume;
    }
    return self;
}

- (NSString *)speak {
    return [NSString stringWithFormat:@"Woof (%.1f)", self.volume];
}

- (void)setVolume:(float)level {
    _volume = level;
}

- (NSString *)describe {
    NSString *base = [super describe];
    return [NSString stringWithFormat:@"%@ [speaks at %.1f]", base, self.volume];
}

@end
