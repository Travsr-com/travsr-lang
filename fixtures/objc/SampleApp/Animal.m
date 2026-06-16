// Base class implementation fixture.
#import "Animal.h"

@implementation Animal

- (instancetype)initWithName:(NSString *)name {
    if (self = [super init]) {
        _name = [name copy];
    }
    return self;
}

- (NSString *)describe {
    return [NSString stringWithFormat:@"Animal: %@", self.name];
}

@end
